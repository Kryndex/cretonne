//! Register pressure tracking.
//!
//! SSA-based register allocation depends on a spilling phase that "lowers register pressure
//! sufficiently". This module defines the data structures needed to measure register pressure
//! accurately enough to guarantee that the coloring phase will not run out of registers.
//!
//! Ideally, measuring register pressure amounts to simply counting the number of live registers at
//! any given program point. This simplistic method has two problems:
//!
//! 1. Registers are not interchangeable. Most ISAs have separate integer and floating-point
//!    register banks, so we need to at least count the number of live registers in each register
//!    bank separately.
//!
//! 2. Some ISAs have complicated register aliasing properties. In particular, the 32-bit ARM
//!    ISA has a floating-point register bank where two 32-bit registers alias one 64-bit register.
//!    This makes it difficult to accurately measure register pressure.
//!
//! This module deals with the problems via *register banks* and *top-level register classes*.
//! Register classes in different register banks are completely independent, so we can count
//! registers in one bank without worrying about the other bank at all.
//!
//! All register classes have a unique top-level register class, and we will count registers for
//! each top-level register class individually. However, a register bank can have multiple
//! top-level register classes that interfere with each other, so all top-level counts need to
//! be considered when determining how many more registers can be allocated.
//!
//! Currently, the only register bank with multiple top-level registers is the `arm32`
//! floating-point register bank which has `S`, `D`, and `Q` top-level classes.

// Remove once we're using the pressure tracker.
#![allow(dead_code)]

use isa::registers::{RegInfo, MAX_TOPRCS, RegClass, RegClassMask};
use regalloc::AllocatableSet;
use std::cmp::min;
use std::iter::ExactSizeIterator;

/// Information per top-level register class.
///
/// Everything but the count is static information computed from the constructor arguments.
#[derive(Default)]
struct TopRC {
    // Number of registers currently used from this register class.
    count: u32,

    // Max number of registers that can be allocated.
    limit: u32,

    // Register units per register.
    width: u8,

    // The first aliasing top-level RC.
    first_toprc: u8,

    // The number of aliasing top-level RCs.
    num_toprcs: u8,
}

pub struct Pressure {
    // Bit mask of top-level register classes that are aliased by other top-level register classes.
    // Unaliased register classes can use a simpler interference algorithm.
    aliased: RegClassMask,

    // Current register counts per top-level register class.
    toprc: [TopRC; MAX_TOPRCS],
}

impl Pressure {
    /// Create a new register pressure tracker.
    pub fn new(reginfo: &RegInfo, usable: &AllocatableSet) -> Pressure {
        let mut p = Pressure {
            aliased: 0,
            toprc: Default::default(),
        };

        // Get the layout of aliasing top-level register classes from the register banks.
        for bank in reginfo.banks {
            let first = bank.first_toprc;
            let num = bank.num_toprcs;
            for rc in &mut p.toprc[first..first + num] {
                rc.first_toprc = first as u8;
                rc.num_toprcs = num as u8;
            }

            // Flag the top-level register classes with aliases.
            if num > 1 {
                p.aliased |= ((1 << num) - 1) << first;
            }
        }

        // Compute per-class limits from `usable`.
        for (toprc, rc) in p.toprc
                .iter_mut()
                .take_while(|t| t.num_toprcs > 0)
                .zip(reginfo.classes) {
            toprc.limit = usable.iter(rc).len() as u32;
            toprc.width = rc.width;
        }

        p
    }

    /// Check for an available register in the register class `rc`.
    ///
    /// If it is possible to allocate one more register from `rc`'s top-level register class,
    /// returns 0.
    ///
    /// If not, returns a bit-mask of top-level register classes that are interfering. Register
    /// pressure should be eased in one of the returned top-level register classes before calling
    /// `can_take()` to check again.
    pub fn check_avail(&self, rc: RegClass) -> RegClassMask {
        let entry = &self.toprc[rc.toprc as usize];
        let mask = 1 << rc.toprc;
        if self.aliased & mask == 0 {
            // This is a simple unaliased top-level register class.
            if entry.count < entry.limit { 0 } else { mask }
        } else {
            // This is the more complicated case. The top-level register class has aliases.
            self.check_avail_aliased(entry)
        }
    }

    /// Check for an available register in a top-level register class that may have aliases.
    ///
    /// This is the out-of-line slow path for `check_avail()`.
    fn check_avail_aliased(&self, entry: &TopRC) -> RegClassMask {
        let first = entry.first_toprc as usize;
        let num = entry.num_toprcs as usize;
        let width = entry.width as u32;
        let ulimit = entry.limit * width;

        // Count up the number of available register units.
        let mut units = 0;
        for (rc, rci) in self.toprc[first..first + num].iter().zip(first..) {
            let rcw = rc.width as u32;
            // If `rc.width` is smaller than `width`, each register in `rc` could potentially block
            // one of ours. This is assuming that none of the smaller registers are straddling the
            // bigger ones.
            //
            // If `rc.width` is larger than `width`, we are also assuming that the registers are
            // aligned and `rc.width` is a multiple of `width`.
            let u = if rcw < width {
                // We can't take more than the total number of register units in the class.
                // This matters for arm32 S-registers which can only ever lock out 16 D-registers.
                min(rc.count * width, rc.limit * rcw)
            } else {
                rc.count * rcw
            };

            // If this top-level RC on its own is responsible for exceeding our limit, return it
            // early to guarantee that registers here are spilled before spilling other registers
            // unnecessarily.
            if u >= ulimit {
                return 1 << rci;
            }

            units += u;
        }

        // We've counted up the worst-case number of register units claimed by all aliasing
        // classes. Compare to the unit limit in this class.
        if units < ulimit {
            0
        } else {
            // Registers need to be spilled from any one of the aliasing classes.
            ((1 << num) - 1) << first
        }
    }

    /// Take a register from `rc`.
    ///
    /// This assumes that `can_take(rc)` already returned 0.
    pub fn take(&mut self, rc: RegClass) {
        self.toprc[rc.toprc as usize].count += 1
    }

    /// Free a register in `rc`.
    pub fn free(&mut self, rc: RegClass) {
        self.toprc[rc.toprc as usize].count -= 1
    }

    /// Reset all counts to 0.
    pub fn reset(&mut self) {
        for e in self.toprc.iter_mut() {
            e.count = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use isa::{TargetIsa, RegClass};
    use regalloc::AllocatableSet;
    use std::borrow::Borrow;
    use super::Pressure;

    // Make an arm32 `TargetIsa`, if possible.
    fn arm32() -> Option<Box<TargetIsa>> {
        use settings;
        use isa;

        let shared_builder = settings::builder();
        let shared_flags = settings::Flags::new(&shared_builder);

        isa::lookup("arm32").map(|b| b.finish(shared_flags))
    }

    // Get a register class by name.
    fn rc_by_name(isa: &TargetIsa, name: &str) -> RegClass {
        isa.register_info()
            .classes
            .iter()
            .find(|rc| rc.name == name)
            .expect("Can't find named register class.")
    }

    #[test]
    fn basic_counting() {
        let isa = arm32().expect("This test requires arm32 support");
        let isa = isa.borrow();
        let gpr = rc_by_name(isa, "GPR");
        let s = rc_by_name(isa, "S");
        let reginfo = isa.register_info();
        let regs = AllocatableSet::new();

        let mut pressure = Pressure::new(&reginfo, &regs);
        let mut count = 0;
        while pressure.check_avail(gpr) == 0 {
            pressure.take(gpr);
            count += 1;
        }
        assert_eq!(count, 16);
        assert_eq!(pressure.check_avail(gpr), 1 << gpr.toprc);
        assert_eq!(pressure.check_avail(s), 0);
        pressure.free(gpr);
        assert_eq!(pressure.check_avail(gpr), 0);
        pressure.take(gpr);
        assert_eq!(pressure.check_avail(gpr), 1 << gpr.toprc);
        assert_eq!(pressure.check_avail(s), 0);
        pressure.reset();
        assert_eq!(pressure.check_avail(gpr), 0);
        assert_eq!(pressure.check_avail(s), 0);
    }

    #[test]
    fn arm_float_bank() {
        let isa = arm32().expect("This test requires arm32 support");
        let isa = isa.borrow();
        let s = rc_by_name(isa, "S");
        let d = rc_by_name(isa, "D");
        let q = rc_by_name(isa, "Q");
        let reginfo = isa.register_info();
        let regs = AllocatableSet::new();

        let mut pressure = Pressure::new(&reginfo, &regs);
        assert_eq!(pressure.check_avail(s), 0);
        assert_eq!(pressure.check_avail(d), 0);
        assert_eq!(pressure.check_avail(q), 0);

        // Allocating a single S-register should not affect availability.
        pressure.take(s);
        assert_eq!(pressure.check_avail(s), 0);
        assert_eq!(pressure.check_avail(d), 0);
        assert_eq!(pressure.check_avail(q), 0);

        pressure.take(d);
        assert_eq!(pressure.check_avail(s), 0);
        assert_eq!(pressure.check_avail(d), 0);
        assert_eq!(pressure.check_avail(q), 0);

        pressure.take(q);
        assert_eq!(pressure.check_avail(s), 0);
        assert_eq!(pressure.check_avail(d), 0);
        assert_eq!(pressure.check_avail(q), 0);

        // Take a total of 16 S-regs.
        for _ in 1..16 {
            pressure.take(s);
        }
        assert_eq!(pressure.check_avail(s), 0);
        assert_eq!(pressure.check_avail(d), 0);
        assert_eq!(pressure.check_avail(q), 0);

        // We've taken 16 S, 1 D, and 1 Q. There should be 6 more Qs.
        for _ in 0..6 {
            assert_eq!(pressure.check_avail(d), 0);
            assert_eq!(pressure.check_avail(q), 0);
            pressure.take(q);
        }

        // We've taken 16 S, 1 D, and 7 Qs.
        assert!(pressure.check_avail(s) != 0);
        assert_eq!(pressure.check_avail(d), 0);
        assert!(pressure.check_avail(q) != 0);
    }
}
