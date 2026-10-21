//! A test that re-runs its own test binary as a child, for a fresh process.

/// Panics unless the child's libtest output says it ran exactly one test, with the child's
/// stdout and stderr in the message.
///
/// A child started with `--exact <name>` runs nothing when `name` matches no test (after a
/// rename, say): it prints `running 0 tests` and exits 0, so a parent that checks only the exit
/// status passes while checking nothing. libtest prints `running N tests` before the first test
/// starts, so this holds for a child that exits or execs from inside the test and never prints
/// a summary.
#[track_caller]
pub fn assert_ran_one_test(stdout: &[u8], stderr: &[u8]) {
    let out = String::from_utf8_lossy(stdout);
    assert!(
        out.lines().any(|line| line == "running 1 test"),
        "the child must run exactly one test; is its --exact name still a test's name?\n\
         --- child stdout\n{out}\n--- child stderr\n{}",
        String::from_utf8_lossy(stderr)
    );
}
