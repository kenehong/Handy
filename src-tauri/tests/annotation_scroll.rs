#[cfg(target_os = "windows")]
#[test]
fn annotation_browses_live_and_intercepts_the_first_drawing_press() {
    let directory = tempfile::tempdir().expect("isolated annotation check directory");
    handy_app_lib::run_annotation_scroll_check(directory.path())
        .expect("live source, input shielding, and first-press recapture must all work");
}
