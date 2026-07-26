use super::*;

/// Builds a model plus a temp dir to hold marker files.
fn model() -> (PendingEditsModel, tempfile::TempDir) {
    (
        PendingEditsModel::new(),
        tempfile::tempdir().expect("temp dir"),
    )
}

#[test]
fn dropping_a_session_writes_the_done_marker() {
    let (mut model, dir) = model();
    let done = dir.path().join("edit.done");

    let session = model.register(PathBuf::from("/tmp/resource.yaml"), done.clone());
    model.forget(Path::new("/tmp/resource.yaml"));
    assert!(!done.exists(), "edit completes only once the session drops");

    drop(session);

    assert_eq!(
        std::fs::read_to_string(&done).expect("done marker written"),
        "0"
    );
}

#[test]
fn a_claimed_session_stays_pending_until_the_claimer_drops_it() {
    let (mut model, dir) = model();
    let done = dir.path().join("edit.done");
    let path = PathBuf::from("/tmp/resource.yaml");

    let registered = model.register(path.clone(), done.clone());
    let claimed = model.claim(&path).expect("session is claimable");

    // The editor tab now owns the session; the requester letting go of its own
    // reference must not complete the edit.
    model.forget(&path);
    drop(registered);
    assert!(!done.exists());

    drop(claimed);
    assert!(done.exists());
}

#[test]
fn an_unclaimed_session_completes_so_the_caller_is_never_stranded() {
    let (mut model, dir) = model();
    let done = dir.path().join("edit.done");
    let path = PathBuf::from("/tmp/resource.yaml");

    // Nothing claims the session — the file turned out not to be openable.
    let registered = model.register(path.clone(), done.clone());
    model.forget(&path);
    drop(registered);

    assert!(
        done.exists(),
        "a file that never opened must still unblock the calling tool"
    );
}

#[test]
fn a_session_can_only_be_claimed_once() {
    let (mut model, dir) = model();
    let path = PathBuf::from("/tmp/resource.yaml");

    let _registered = model.register(path.clone(), dir.path().join("edit.done"));

    assert!(model.claim(&path).is_some());
    assert!(model.claim(&path).is_none());
}

#[test]
fn claiming_an_unrelated_path_finds_nothing() {
    let (mut model, dir) = model();

    let _registered = model.register(
        PathBuf::from("/tmp/resource.yaml"),
        dir.path().join("edit.done"),
    );

    assert!(model.claim(Path::new("/tmp/other.yaml")).is_none());
}

#[test]
fn re_registering_a_path_completes_the_earlier_edit() {
    let (mut model, dir) = model();
    let path = PathBuf::from("/tmp/resource.yaml");
    let first_done = dir.path().join("first.done");
    let second_done = dir.path().join("second.done");

    let first = model.register(path.clone(), first_done.clone());
    let second = model.register(path.clone(), second_done.clone());
    drop(first);

    assert!(
        first_done.exists(),
        "the superseded edit must not stay blocked"
    );
    assert!(!second_done.exists());

    model.forget(&path);
    drop(second);
    assert!(second_done.exists());
}
