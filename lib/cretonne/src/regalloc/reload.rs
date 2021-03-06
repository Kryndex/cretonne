//! Reload pass
//!
//! The reload pass runs between the spilling and coloring passes. Its primary responsibility is to
//! insert `spill` and `fill` instructions such that instruction operands expecting a register will
//! get a value with register affinity, and operands expecting a stack slot will get a value with
//! stack affinity.
//!
//! The secondary responsibility of the reload pass is to reuse values in registers as much as
//! possible to minimize the number of `fill` instructions needed. This must not cause the register
//! pressure limits to be exceeded.

use dominator_tree::DominatorTree;
use ir::{Ebb, Inst, Value, Function, DataFlowGraph};
use ir::layout::{Cursor, CursorPosition};
use ir::{InstBuilder, ArgumentLoc};
use isa::RegClass;
use isa::{TargetIsa, Encoding, EncInfo, ConstraintKind};
use regalloc::affinity::Affinity;
use regalloc::live_value_tracker::{LiveValue, LiveValueTracker};
use regalloc::liveness::Liveness;
use sparse_map::{SparseMap, SparseMapValue};
use topo_order::TopoOrder;

/// Reusable data structures for the reload pass.
pub struct Reload {
    candidates: Vec<ReloadCandidate>,
    reloads: SparseMap<Value, ReloadedValue>,
}

/// Context data structure that gets instantiated once per pass.
struct Context<'a> {
    // Cached ISA information.
    // We save it here to avoid frequent virtual function calls on the `TargetIsa` trait object.
    encinfo: EncInfo,

    // References to contextual data structures we need.
    domtree: &'a DominatorTree,
    liveness: &'a mut Liveness,
    topo: &'a mut TopoOrder,

    candidates: &'a mut Vec<ReloadCandidate>,
    reloads: &'a mut SparseMap<Value, ReloadedValue>,
}

impl Reload {
    /// Create a new blank reload pass.
    pub fn new() -> Reload {
        Reload {
            candidates: Vec::new(),
            reloads: SparseMap::new(),
        }
    }

    /// Run the reload algorithm over `func`.
    pub fn run(&mut self,
               isa: &TargetIsa,
               func: &mut Function,
               domtree: &DominatorTree,
               liveness: &mut Liveness,
               topo: &mut TopoOrder,
               tracker: &mut LiveValueTracker) {
        let mut ctx = Context {
            encinfo: isa.encoding_info(),
            domtree,
            liveness,
            topo,
            candidates: &mut self.candidates,
            reloads: &mut self.reloads,
        };
        ctx.run(func, tracker)
    }
}

/// A reload candidate.
///
/// This represents a stack value that is used by the current instruction where a register is
/// needed.
struct ReloadCandidate {
    value: Value,
    regclass: RegClass,
}

/// A Reloaded value.
///
/// This represents a value that has been reloaded into a register value from the stack.
struct ReloadedValue {
    stack: Value,
    reg: Value,
}

impl SparseMapValue<Value> for ReloadedValue {
    fn key(&self) -> Value {
        self.stack
    }
}

impl<'a> Context<'a> {
    fn run(&mut self, func: &mut Function, tracker: &mut LiveValueTracker) {
        self.topo.reset(func.layout.ebbs());
        while let Some(ebb) = self.topo.next(&func.layout, self.domtree) {
            self.visit_ebb(ebb, func, tracker);
        }
    }

    fn visit_ebb(&mut self, ebb: Ebb, func: &mut Function, tracker: &mut LiveValueTracker) {
        dbg!("Reloading {}:", ebb);
        let start_from = self.visit_ebb_header(ebb, func, tracker);
        tracker.drop_dead_args();

        let mut pos = Cursor::new(&mut func.layout);
        pos.set_position(start_from);
        while let Some(inst) = pos.current_inst() {
            let encoding = func.encodings[inst];
            if encoding.is_legal() {
                self.visit_inst(ebb, inst, encoding, &mut pos, &mut func.dfg, tracker);
                tracker.drop_dead(inst);
            } else {
                pos.next_inst();
            }
        }
    }

    /// Process the EBB parameters. Return the next instruction in the EBB to be processed
    fn visit_ebb_header(&self,
                        ebb: Ebb,
                        func: &mut Function,
                        tracker: &mut LiveValueTracker)
                        -> CursorPosition {
        let (liveins, args) =
            tracker.ebb_top(ebb, &func.dfg, self.liveness, &func.layout, self.domtree);

        if func.layout.entry_block() == Some(ebb) {
            assert_eq!(liveins.len(), 0);
            self.visit_entry_args(ebb, func, args)
        } else {
            self.visit_ebb_args(ebb, func, args)
        }
    }

    /// Visit the arguments to the entry block.
    /// These values have ABI constraints from the function signature.
    fn visit_entry_args(&self,
                        ebb: Ebb,
                        func: &mut Function,
                        args: &[LiveValue])
                        -> CursorPosition {
        assert_eq!(func.signature.argument_types.len(), args.len());
        let mut pos = Cursor::new(&mut func.layout);
        pos.goto_top(ebb);
        pos.next_inst();

        for (abi, arg) in func.signature.argument_types.iter().zip(args) {
            match abi.location {
                ArgumentLoc::Reg(_) => {
                    if arg.affinity.is_stack() {
                        // An incoming register parameter was spilled. Replace the parameter value
                        // with a temporary register value that is immediately spilled.
                        let reg = func.dfg.replace_ebb_arg(arg.value, abi.value_type);
                        func.dfg.ins(&mut pos).with_result(arg.value).spill(reg);
                        // TODO: Update live ranges.
                    }
                }
                ArgumentLoc::Stack(_) => {
                    assert!(arg.affinity.is_stack());
                }
                ArgumentLoc::Unassigned => panic!("Unexpected ABI location"),
            }
        }
        pos.position()
    }

    fn visit_ebb_args(&self, ebb: Ebb, func: &mut Function, _args: &[LiveValue]) -> CursorPosition {
        let mut pos = Cursor::new(&mut func.layout);
        pos.goto_top(ebb);
        pos.next_inst();
        pos.position()
    }

    /// Process the instruction pointed to by `pos`, and advance the cursor to the next instruction
    /// that needs processing.
    fn visit_inst(&mut self,
                  ebb: Ebb,
                  inst: Inst,
                  encoding: Encoding,
                  pos: &mut Cursor,
                  dfg: &mut DataFlowGraph,
                  tracker: &mut LiveValueTracker) {
        // Get the operand constraints for `inst` that we are trying to satisfy.
        let constraints = self.encinfo
            .operand_constraints(encoding)
            .expect("Missing instruction encoding");

        assert!(self.candidates.is_empty());

        // Identify reload candidates.
        for (op, &arg) in constraints.ins.iter().zip(dfg.inst_args(inst)) {
            if op.kind != ConstraintKind::Stack {
                let lv = self.liveness.get(arg).expect("Missing live range for arg");
                if lv.affinity.is_stack() {
                    self.candidates
                        .push(ReloadCandidate {
                                  value: arg,
                                  regclass: op.regclass,
                              })
                }
            }
        }

        // Insert fill instructions before `inst`.
        while let Some(cand) = self.candidates.pop() {
            if let Some(_reload) = self.reloads.get_mut(cand.value) {
                continue;
            }

            let reg = dfg.ins(pos).fill(cand.value);
            self.reloads
                .insert(ReloadedValue {
                            stack: cand.value,
                            reg: reg,
                        });

            // Create a live range for the new reload.
            let affinity = Affinity::Reg(cand.regclass.into());
            self.liveness.create_dead(reg, dfg.value_def(reg), affinity);
            self.liveness.extend_locally(reg, ebb, inst, &pos.layout);
        }

        // Rewrite arguments.
        for arg in dfg.inst_args_mut(inst) {
            if let Some(reload) = self.reloads.get(*arg) {
                *arg = reload.reg;
            }
        }

        // TODO: Reuse reloads for future instructions.
        self.reloads.clear();

        let (_throughs, _kills, defs) = tracker.process_inst(inst, dfg, self.liveness);

        // Advance to the next instruction so we can insert any spills after the instruction.
        pos.next_inst();

        // Rewrite register defs that need to be spilled.
        //
        // Change:
        //
        // v2 = inst ...
        //
        // Into:
        //
        // v7 = inst ...
        // v2 = spill v7
        //
        // That way, we don't need to rewrite all future uses of v2.
        for (lv, op) in defs.iter().zip(constraints.outs) {
            if lv.affinity.is_stack() && op.kind != ConstraintKind::Stack {
                let value_type = dfg.value_type(lv.value);
                let reg = dfg.replace_result(lv.value, value_type);
                dfg.ins(pos).with_result(lv.value).spill(reg);
                let spill = dfg.value_def(lv.value).unwrap_inst();

                // Create a live range for reg.
                self.liveness.create_dead(reg, inst, Affinity::new(op));
                self.liveness.extend_locally(reg, ebb, spill, &pos.layout);
                self.liveness.move_def_locally(lv.value, spill);
            }
        }
    }
}
