use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, str};

use crate::base::log::CommandLogExt;
use crate::base::{Error, Result};
use crate::domain::executable::SearchPaths;

use tempfile::NamedTempFile;

static RESOLVER_SOURCE_CODE: &str = r"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <link.h>

#include <stdio.h>

int main(int argc, char** argv) {
  char* name = argv[1];
  void* handle = dlopen(name, RTLD_LAZY);
  if (handle == NULL) {
    fputs(dlerror(), stderr);
    return 1;
  }
  struct link_map* link_map;
  if (dlinfo(handle, RTLD_DI_LINKMAP, &link_map) != 0) {
    fputs(dlerror(), stderr);
    return 2;
  }
  puts(link_map->l_name);
  dlclose(handle);
}";

#[derive(Debug)]
pub struct Resolver<'a> {
    program_path: PathBuf,
    search_paths: &'a SearchPaths,
}

impl<'a> Resolver<'a> {
    pub fn new<P, Q>(interp: P, search_paths: &'a SearchPaths, cc_path: Q) -> Result<Self>
    where
        P: AsRef<Path>,
        Q: AsRef<Path>,
    {
        let program_path = calc_program_path(&interp, &cc_path);
        if !program_path.exists() {
            build_resolver_program(&program_path, &interp, &cc_path)?;
        }

        let resolver = Resolver {
            program_path,
            search_paths,
        };

        tracing::debug!(?resolver, "resolver: created resolver");
        Ok(resolver)
    }

    // lookup_rpath --> lookup_env --> lookup_runpath --> lookup_rest
    // TODO: take secure-execution mode into consideration
    pub fn lookup(&self, name: &str) -> Result<PathBuf> {
        if let Some(path) = self.lookup_rpath(name) {
            tracing::debug!(%name, path = %path.display(), "resolver: found by Rpath");
            return Ok(path);
        }

        if let Some(path) = self.lookup_env(name) {
            tracing::debug!(
                %name,
                path = %path.display(),
                "resolver: found by LD_LIBRARY_PATH",
            );
            return Ok(path);
        }

        if let Some(path) = self.lookup_runpath(name) {
            tracing::debug!(%name, path = %path.display(), "resolver: found by RunPath");
            return Ok(path);
        }

        let path = self.lookup_rest(name)?;
        tracing::debug!(%name, path = %path.display(), "resolver: found by ld.so");

        Ok(path)
    }

    fn lookup_rpath(&self, name: &str) -> Option<PathBuf> {
        if self.search_paths.runpath().is_some() {
            return None;
        }

        self.search_paths
            .iter_rpaths()
            .find_map(|x| try_joined(x, name))
    }

    fn lookup_runpath(&self, name: &str) -> Option<PathBuf> {
        self.search_paths
            .iter_runpaths()
            .find_map(|x| try_joined(x, name))
    }

    fn lookup_env(&self, name: &str) -> Option<PathBuf> {
        self.search_paths
            .iter_ld_library_paths()
            .find_map(|x| try_joined(x, name))
    }

    fn lookup_rest(&self, name: &str) -> Result<PathBuf> {
        let output = Command::new(&self.program_path)
            .arg(name)
            .env_clear()
            .output_with_log()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(Error::SharedLibraryLookup(stderr));
        }

        Ok(str::from_utf8(&output.stdout)?.trim().to_string().into())
    }
}

fn calc_program_path<P, Q>(interp: P, cc_path: Q) -> PathBuf
where
    P: AsRef<Path>,
    Q: AsRef<Path>,
{
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let interp_hash = {
        let mut s = DefaultHasher::new();
        interp.as_ref().hash(&mut s);
        s.finish()
    };

    let cc_path_hash = {
        let mut s = DefaultHasher::new();
        cc_path.as_ref().hash(&mut s);
        s.finish()
    };

    let mut path = env::temp_dir();
    path.push(format!(
        "magicpak_resolver_{}_{}",
        interp_hash, cc_path_hash
    ));
    path
}

fn build_resolver_program<P, Q, R>(program_path: P, interp: Q, cc_path: R) -> Result<()>
where
    P: AsRef<Path>,
    Q: AsRef<Path>,
    R: AsRef<Path>,
{
    let mut source = NamedTempFile::new()?;
    write!(source, "{}", RESOLVER_SOURCE_CODE)?;
    let source_path = source.into_temp_path();

    let output = Command::new(cc_path.as_ref())
        .arg("-xc")
        .arg(&source_path)
        .arg(format!("-Wl,-dynamic-linker,{}", interp.as_ref().display()))
        .arg("-ldl")
        .arg("-o")
        .arg(program_path.as_ref())
        .output_with_log()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(Error::ResolverCompilation(stderr));
    }
    source_path.close()?;
    Ok(())
}

fn try_joined<P, Q>(path1: P, path2: Q) -> Option<PathBuf>
where
    P: AsRef<Path>,
    Q: AsRef<Path>,
{
    let joined = path1.as_ref().join(path2);
    if joined.exists() {
        Some(joined)
    } else {
        None
    }
}
