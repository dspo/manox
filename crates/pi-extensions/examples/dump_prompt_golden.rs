//! Regenerate `testdata/captain_prompt.golden.txt` from the current
//! template + prose:
//!
//! ```sh
//! cargo run -p pi-extensions --example dump_prompt_golden \
//!   > crates/pi-extensions/testdata/captain_prompt.golden.txt
//! ```
//!
//! The dump renders [`pi_extensions::prompt::render_golden_fixture`] — the
//! exact fixture the `captain_prompt_matches_golden_bytes` test asserts
//! against — so regeneration cannot drift from the test's expectation.

fn main() {
    print!("{}", pi_extensions::prompt::render_golden_fixture());
}
