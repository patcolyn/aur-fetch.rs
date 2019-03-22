use crate::{Callback, Error};

use std::cell::RefCell;
use std::env;
use std::ffi::OsStr;
use std::fs::{create_dir_all, File};
use std::io::{self, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::rc::Rc;

use futures::future::{self, Future};
use tempdir::TempDir;
use tokio_process::CommandExt;
use url::Url;

/// Result type for this crate;
pub type Result<T> = std::result::Result<T, Error>;

/// Handle to the current configuration.
///
/// This handle is used to configure parts of the fetching process. All the features of this crate
/// must be done through this handle.
#[derive(Clone, Debug)]
pub struct Handle {
    /// The directory to place AUR packages in.
    pub clone_dir: PathBuf,
    /// The directory to place diffs in.
    pub diff_dir: PathBuf,
    /// The git command to run.
    pub git: PathBuf,
    /// The AUR URL.
    pub aur_url: Url,
}

impl Handle {
    /// Create a new Handle with working defaults.
    ///
    /// This Inializes the clone and diff dir to the current dirrectory. If you want to configure
    /// a cache directory you will need to do that yourself.
    pub fn new() -> Result<Self> {
        Ok(Self {
            clone_dir: env::current_dir()?,
            diff_dir: env::current_dir()?,
            git: "git".into(),
            aur_url: "https://aur.archlinux.org".parse().unwrap(),
        })
    }

    /// Downloads a list of packages to the cache dir.
    ///
    /// This downloads packages from the AUR using git. New packages will be cloned, while packages
    /// that already exist in cache will be fetched. Merging will need to be done in a separate
    /// step.
    ///
    /// Each package is downloaded concurrently which givess a major speedup. No other functions
    /// are run concirrently as they will all complete pretty much isntantly.
    ///
    /// Depending on how many packages are being downloaded and connection speed this
    /// function may take a little while to complete. See [`download_cb`](fn.download_cb.html) if
    /// you wish track the progress of each download.
    ///
    /// This also filters the input list to packages that were already in cache. This filtered list
    /// can then be passed on to [`needs_merge`](fn.needs_merge.html) as freshly cloned packages will
    /// not need to be merged.
    pub fn download<'a, S: AsRef<str>>(&self, pkgs: &'a [S]) -> Result<Vec<&'a str>> {
        self.download_cb(pkgs, |_| ())
    }

    /// The same as [`download`](fn.download.html) but calls a Callback after each download.
    ///
    /// The callback is called each time a package download is completed.
    pub fn download_cb<'a, S: AsRef<str>, F: Fn(Callback)>(
        &self,
        pkgs: &'a [S],
        f: F,
    ) -> Result<Vec<&'a str>> {
        let mut fetched = Vec::new();
        self.mk_clone_dir()?;

        // bad, need to learn how to use futures properly
        let f = Rc::new(f);
        let n = Rc::new(RefCell::new(0));

        let pkgs = pkgs.iter().map(|pkg| {
            let mut url = self.aur_url.clone();
            let pkg = pkg.as_ref();
            url.set_path(pkg.as_ref());

            let command = if self.is_git_repo(pkg) {
                fetched.push(pkg);
                Command::new(&self.git)
                    .current_dir(&self.clone_dir.join(pkg))
                    .args(&["fetch", "-v"])
                    .output_async()
            } else {
                Command::new(&self.git)
                    .current_dir(&self.clone_dir)
                    .args(&["clone", "--no-progress", url.as_str()])
                    .output_async()
            };

            // bad, need to learn how to use futures properly
            let pkg = pkg.to_string();
            let f = f.clone();
            let n = n.clone();

            command.then(move |r| {
                let mut n = n.borrow_mut();
                *n += 1;
                f(Callback { pkg, n: *n });
                let r = r.map_err(Error::from);

                match r {
                    Ok(ref o) if !o.status.success() => future::err(Error::CommandFailed(
                        String::from_utf8_lossy(&o.stderr).into(),
                    )),
                    _ => future::result(r),
                }
            })
        });

        future::join_all(pkgs).wait()?;
        Ok(fetched)
    }

    /// Filters a list of packages, keeping ones that need to be merged.
    ///
    /// Needing to be merged is defined as the current HEAD being different to the upstram HEAD.
    pub fn needs_merge<'a, S: AsRef<str>>(&self, pkgs: &'a [S]) -> Result<Vec<&'a str>> {
        let mut ret = Vec::new();

        for pkg in pkgs {
            if git_needs_merge(&self.git, self.clone_dir.join(pkg.as_ref()))? {
                ret.push(pkg.as_ref());
            }
        }

        Ok(ret)
    }

    /// Diff a list of packages returning the diffs as strings.
    ///
    /// Additionally this function gives you the ability to force color. This is useful if you
    /// intend to print the diffs to stdout.
    pub fn diff<S: AsRef<str>>(&self, pkgs: &[S], color: bool) -> Result<Vec<String>> {
        let pkgs = pkgs.iter();
        let mut ret = Vec::new();

        for pkg in pkgs {
            let output = git_log(&self.git, self.clone_dir.join(pkg.as_ref()), color)?;
            let mut s: String = String::from_utf8_lossy(&output.stdout).into();
            let output = git_diff(&self.git, self.clone_dir.join(pkg.as_ref()), color)?;
            s.push_str(&String::from_utf8_lossy(&output.stdout));
            s.push('\n');
            ret.push(s);
        }

        Ok(ret)
    }

    /// Diff a list of packages and save them to diff_dir.
    pub fn save_diffs<S: AsRef<str>>(&self, pkgs: &[S]) -> Result<()> {
        self.mk_diff_dir()?;

        for pkg in pkgs {
            let mut path = self.diff_dir.join(pkg.as_ref());
            path.set_extension("diff");

            let mut file = File::create(path)?;

            file.write_all(&git_log(&self.git, self.clone_dir.join(pkg.as_ref()), false)?.stdout)?;
            file.write_all(&[b'\n'])?;
            file.write_all(&git_diff(&self.git, self.clone_dir.join(pkg.as_ref()), false)?.stdout)?;
        }

        Ok(())
    }

    /// Makes a view of newly downloaded files.
    ///
    /// This view is a tmp dir containing the packages downloaded/fetched and diffs
    /// for packages that have diffs.
    ///
    /// Files are symlinked from the cache dirs so there is no duplication of files.
    pub fn make_view<S1: AsRef<str>, S2: AsRef<str>>(
        &self,
        pkgs: &[S1],
        diffs: &[S2],
    ) -> Result<TempDir> {
        let tmp = TempDir::new("aur")?;

        for pkg in diffs {
            let dest = tmp.path().join(pkg.as_ref()).with_extension("diff");
            let src = self.diff_dir.join(pkg.as_ref()).with_extension("diff");
            if src.is_file() {
                symlink(src, &dest)?;
            }
        }

        for pkg in pkgs {
            let mut dest = tmp.path().join(pkg.as_ref());

            let src = self.clone_dir.join(pkg.as_ref());
            if src.is_dir() {
                symlink(src, &dest)?;
            }

            let src = self.clone_dir.join(pkg.as_ref()).join("PKGBUILD");
            dest.set_extension("PKGBUILD");
            if src.is_file() {
                symlink(src, &dest)?;
            }

            let src = self.clone_dir.join(pkg.as_ref()).join("SRCINFO");
            dest.set_extension("SRCINFO");
            if src.is_file() {
                symlink(src, &dest)?;
            }
        }

        Ok(tmp)
    }

    /// Merge a list of packages with their upstream.
    pub fn merge<S: AsRef<str>>(&self, pkgs: &[S]) -> Result<()> {
        let pkgs = pkgs.iter();

        for pkg in pkgs {
            let path = self.clone_dir.join(pkg.as_ref());
            git_rebase(&self.git, path)?;
        }

        Ok(())
    }

    fn is_git_repo<S: AsRef<str>>(&self, pkg: S) -> bool {
        self.clone_dir.join(pkg.as_ref()).join(".git").is_dir()
    }

    fn mk_clone_dir(&self) -> io::Result<()> {
        create_dir_all(&self.clone_dir)
    }

    fn mk_diff_dir(&self) -> io::Result<()> {
        create_dir_all(&self.diff_dir)
    }
}

fn git_command<S: AsRef<OsStr>, P: AsRef<Path>>(git: S, path: P, args: &[&str]) -> Result<Output> {
    let output = Command::new(git.as_ref())
        .current_dir(path.as_ref())
        .args(args)
        .output()?;

    if output.status.success() {
        Ok(output)
    } else {
        Err(Error::CommandFailed(
            String::from_utf8_lossy(&output.stderr).into(),
        ))
    }
}

fn git_rebase<S: AsRef<OsStr>, P: AsRef<Path>>(git: S, path: P) -> Result<Output> {
    git_command(&git, &path, &["reset", "--hard", "-q", "HEAD"])?;
    Ok(git_command(&git, &path, &["rebase"])?)
}

fn git_needs_merge<S: AsRef<OsStr>, P: AsRef<Path>>(git: S, path: P) -> Result<bool> {
    let output = git_command(git, path, &["rev-parse", "HEAD", "HEAD@{u}"])?;
    let s = String::from_utf8_lossy(&output.stdout);
    let mut s = s.split('\n');

    let head = s.next().unwrap();
    let upstream = s.next().unwrap();

    Ok(head != upstream)
}

fn git_log<S: AsRef<OsStr>, P: AsRef<Path>>(git: S, path: P, color: bool) -> Result<Output> {
    let color = if color {
        "--color=always"
    } else {
        "--color=never"
    };
    Ok(git_command(git, path, &["log", "..HEAD@{u}", color])?)
}

fn git_diff<S: AsRef<OsStr>, P: AsRef<Path>>(git: S, path: P, color: bool) -> Result<Output> {
    let color = if color {
        "--color=always"
    } else {
        "--color=never"
    };
    git_command(&git, &path, &["reset", "--hard", "HEAD"])?;
    git_command(
        &git,
        &path,
        &[
            "-c",
            "user.email=aur",
            "-c",
            "user.name=aur",
            "merge",
            "--no-edit",
            "--no-ff",
            "--no-commit",
        ],
    )?;
    Ok(git_command(
        &git,
        &path,
        &["diff", "--stat", "--patch", "--cached", color],
    )?)
}