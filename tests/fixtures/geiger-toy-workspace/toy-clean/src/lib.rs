// toy-clean: zero unsafe. Geiger should report all five categories
// at 0 for this crate. Paired with toy-unsafe to exercise the
// workspace-member enumeration in scripts/geiger-update-baseline.sh.

pub fn add(a: u32, b: u32) -> u32 {
    a.wrapping_add(b)
}
