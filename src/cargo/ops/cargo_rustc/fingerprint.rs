use std::hash::{Hash, Hasher};
use std::hash::sip::SipHasher;
use std::io::{fs, File, UserRWX};

use core::{Package, Target};
use util;
use util::hex::short_hash;
use util::{CargoResult, Fresh, Dirty, Freshness, internal, Require, profile};

use super::{Kind, KindTarget};
use super::job::Work;
use super::context::Context;

/// A tuple result of the `prepare_foo` functions in this module.
///
/// The first element of the triple is whether the target in question is
/// currently fresh or not, and the second two elements are work to perform when
/// the target is dirty or fresh, respectively.
///
/// Both units of work are always generated because a fresh package may still be
/// rebuilt if some upstream dependency changes.
pub type Preparation = (Freshness, Work, Work);

/// Prepare the necessary work for the fingerprint for a specific target.
///
/// When dealing with fingerprints, cargo gets to choose what granularity
/// "freshness" is considered at. One option is considering freshness at the
/// package level. This means that if anything in a package changes, the entire
/// package is rebuilt, unconditionally. This simplicity comes at a cost,
/// however, in that test-only changes will cause libraries to be rebuilt, which
/// is quite unfortunate!
///
/// The cost was deemed high enough that fingerprints are now calculated at the
/// layer of a target rather than a package. Each target can then be kept track
/// of separately and only rebuilt as necessary. This requires cargo to
/// understand what the inputs are to a target, so we drive rustc with the
/// --dep-info flag to learn about all input files to a unit of compilation.
///
/// This function will calculate the fingerprint for a target and prepare the
/// work necessary to either write the fingerprint or copy over all fresh files
/// from the old directories to their new locations.
pub fn prepare_target(cx: &mut Context, pkg: &Package, target: &Target,
                      kind: Kind) -> CargoResult<Preparation> {
    let _p = profile::start(format!("fingerprint: {} / {}",
                                    pkg.get_package_id(), target));
    let (old, new) = dirs(cx, pkg, kind);
    let filename = if target.is_lib() {
        format!("lib-{}", target.get_name())
    } else if target.get_profile().is_doc() {
        format!("doc-{}", target.get_name())
    } else {
        format!("bin-{}", target.get_name())
    };
    let old_loc = old.join(filename.as_slice());
    let new_loc = new.join(filename.as_slice());

    let new_fingerprint = try!(calculate_target_fingerprint(cx, pkg, target));
    let new_fingerprint = mk_fingerprint(cx, &new_fingerprint);

    let is_fresh = try!(is_fresh(&old_loc, new_fingerprint.as_slice()));
    let layout = cx.layout(kind);
    let mut pairs = vec![(old_loc, new_loc.clone())];

    if !target.get_profile().is_doc() {
        pairs.extend(cx.target_filenames(target).iter().map(|filename| {
            let filename = filename.as_slice();
            ((layout.old_root().join(filename), layout.root().join(filename)))
        }));
    }

    Ok(prepare(is_fresh, new_loc, new_fingerprint, pairs))
}

/// Prepare the necessary work for the fingerprint of a build command.
///
/// Build commands are located on packages, not on targets. Additionally, we
/// don't have --dep-info to drive calculation of the fingerprint of a build
/// command. This brings up an interesting predicament which gives us a few
/// options to figure out whether a build command is dirty or not:
///
/// 1. A build command is dirty if *any* file in a package changes. In theory
///    all files are candidate for being used by the build command.
/// 2. A build command is dirty if any file in a *specific directory* changes.
///    This may lose information as it may require files outside of the specific
///    directory.
/// 3. A build command must itself provide a dep-info-like file stating how it
///    should be considered dirty or not.
///
/// The currently implemented solution is option (1), although it is planned to
/// migrate to option (2) in the near future.
pub fn prepare_build_cmd(cx: &mut Context, pkg: &Package)
                         -> CargoResult<Preparation> {
    let _p = profile::start(format!("fingerprint build cmd: {}",
                                    pkg.get_package_id()));

    // TODO: this should not explicitly pass KindTarget
    let kind = KindTarget;

    if pkg.get_manifest().get_build().len() == 0 {
        return Ok((Fresh, proc() Ok(()), proc() Ok(())))
    }
    let (old, new) = dirs(cx, pkg, kind);
    let old_loc = old.join("build");
    let new_loc = new.join("build");

    let new_fingerprint = try!(calculate_build_cmd_fingerprint(cx, pkg));
    let new_fingerprint = mk_fingerprint(cx, &new_fingerprint);

    let is_fresh = try!(is_fresh(&old_loc, new_fingerprint.as_slice()));
    let layout = cx.layout(kind);
    let pairs = vec![(old_loc, new_loc.clone()),
                     (layout.old_native(pkg), layout.native(pkg))];

    Ok(prepare(is_fresh, new_loc, new_fingerprint, pairs))
}

/// Prepare work for when a package starts to build
pub fn prepare_init(cx: &mut Context, pkg: &Package, kind: Kind)
                    -> (Work, Work) {
    let (_, new1) = dirs(cx, pkg, kind);
    let new2 = new1.clone();

    let work1 = proc() { try!(fs::mkdir(&new1, UserRWX)); Ok(()) };
    let work2 = proc() { try!(fs::mkdir(&new2, UserRWX)); Ok(()) };

    (work1, work2)
}

/// Given the data to build and write a fingerprint, generate some Work
/// instances to actually perform the necessary work.
fn prepare(is_fresh: bool, loc: Path, fingerprint: String,
           to_copy: Vec<(Path, Path)>) -> Preparation {
    let write_fingerprint = proc() {
        try!(File::create(&loc).write_str(fingerprint.as_slice()));
        Ok(())
    };

    let move_old = proc() {
        for &(ref src, ref dst) in to_copy.iter() {
            try!(fs::rename(src, dst));
        }
        Ok(())
    };

    (if is_fresh {Fresh} else {Dirty}, write_fingerprint, move_old)
}

/// Return the (old, new) location for fingerprints for a package
pub fn dirs(cx: &mut Context, pkg: &Package, kind: Kind) -> (Path, Path) {
    let dirname = format!("{}-{}", pkg.get_name(),
                          short_hash(pkg.get_package_id()));
    let dirname = dirname.as_slice();
    let layout = cx.layout(kind);
    let layout = layout.proxy();
    (layout.old_fingerprint().join(dirname), layout.fingerprint().join(dirname))
}

fn is_fresh(loc: &Path, new_fingerprint: &str) -> CargoResult<bool> {
    let mut file = match File::open(loc) {
        Ok(file) => file,
        Err(..) => return Ok(false),
    };

    let old_fingerprint = try!(file.read_to_string());

    log!(5, "old fingerprint: {}", old_fingerprint);
    log!(5, "new fingerprint: {}", new_fingerprint);

    Ok(old_fingerprint.as_slice() == new_fingerprint)
}

/// Frob in the necessary data from the context to generate the real
/// fingerprint.
fn mk_fingerprint<T: Hash>(cx: &Context, data: &T) -> String {
    let hasher = SipHasher::new_with_keys(0,0);
    util::to_hex(hasher.hash(&(&cx.rustc_version, data)))
}

fn calculate_target_fingerprint(cx: &Context, pkg: &Package, target: &Target)
                                -> CargoResult<String> {
    let source = cx.sources
        .get(pkg.get_package_id().get_source_id())
        .expect("BUG: Missing package source");

    let pkg_fingerprint = try!(source.fingerprint(pkg));
    Ok(pkg_fingerprint + short_hash(target))
}

fn calculate_build_cmd_fingerprint(cx: &Context, pkg: &Package)
                                   -> CargoResult<String> {
    let source = cx.sources
        .get(pkg.get_package_id().get_source_id())
        .expect("BUG: Missing package source");

    source.fingerprint(pkg)
}
