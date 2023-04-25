// Copyright 2023 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The execution phase is implemented by this module.
//!
//! The result of the execution phase is [Session], which contains one or more
//! [Segment]s, each which contains an execution trace of the specified program.

mod env;
pub(crate) mod io;
mod monitor;
#[cfg(feature = "profiler")]
pub(crate) mod profiler;
#[cfg(test)]
mod tests;

use std::{array, cell::RefCell, fmt::Debug, io::Write, mem::take, rc::Rc};

use anyhow::{anyhow, bail, Context, Result};
use risc0_zkp::{
    core::{
        digest::{DIGEST_BYTES, DIGEST_WORDS},
        hash::sha::{BLOCK_BYTES, BLOCK_WORDS},
        log2_ceil,
    },
    ZK_CYCLES,
};
use risc0_zkvm_platform::{
    fileno,
    memory::MEM_SIZE,
    syscall::{
        ecall, halt,
        reg_abi::{REG_A0, REG_A1, REG_A2, REG_A3, REG_A4, REG_T0},
    },
    PAGE_SIZE, WORD_SIZE,
};
use rrs_lib::{instruction_executor::InstructionExecutor, HartState};
use serde::{Deserialize, Serialize};

pub use self::env::{ExecutorEnv, ExecutorEnvBuilder};
use self::monitor::MemoryMonitor;
use crate::{
    align_up, bonsai_api,
    opcode::{MajorType, OpCode},
    ExitCode, Loader, MemoryImage, Program, Segment, Session,
};

/// The number of cycles required to compress a SHA-256 block.
const SHA_CYCLES: usize = 72;

/// The Executor provides an implementation for the execution phase.
///
/// The proving phase uses an execution trace generated by the Executor.
pub struct Executor<'a> {
    env: ExecutorEnv<'a>,
    pre_image: MemoryImage,
    monitor: MemoryMonitor,
    pre_pc: u32,
    pc: u32,
    init_cycles: usize,
    fini_cycles: usize,
    body_cycles: usize,
    segment_cycle: usize,
    segments: Vec<Segment>,
    insn_counter: u32,
    bonsai_proof_id: Option<i64>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SyscallRecord {
    pub to_guest: Vec<u32>,
    pub regs: (u32, u32),
}

#[derive(Clone)]
pub struct OpCodeResult {
    pc: u32,
    exit_code: Option<ExitCode>,
    extra_cycles: usize,
    syscall: Option<SyscallRecord>,
}

impl OpCodeResult {
    fn new(
        pc: u32,
        exit_code: Option<ExitCode>,
        extra_cycles: usize,
        syscall: Option<SyscallRecord>,
    ) -> Self {
        Self {
            pc,
            exit_code,
            extra_cycles,
            syscall,
        }
    }
}

// Capture the journal output in a buffer that we can access afterwards.
#[derive(Clone, Default)]
struct Journal {
    buf: Rc<RefCell<Vec<u8>>>,
}

impl Write for Journal {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf.borrow_mut().write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.buf.borrow_mut().flush()
    }
}

impl<'a> Executor<'a> {
    /// Construct a new [Executor] from a [MemoryImage] and entry point.
    pub fn new(env: ExecutorEnv<'a>, image: MemoryImage, pc: u32) -> Self {
        Self::new_with_id(env, image, pc, None)
    }

    /// Construct a new [Executor] from a [MemoryImage], entry point, along with
    /// the proof ID generated from bonsai
    fn new_with_id(
        env: ExecutorEnv<'a>,
        image: MemoryImage,
        pc: u32,
        bonsai_proof_id: Option<i64>,
    ) -> Self {
        let pre_image = image.clone();
        let monitor = MemoryMonitor::new(image);
        let loader = Loader::new();
        let init_cycles = loader.init_cycles();
        let fini_cycles = loader.fini_cycles();

        Self {
            env,
            pre_image,
            monitor,
            pre_pc: pc,
            pc,
            init_cycles,
            fini_cycles,
            body_cycles: 0,
            segment_cycle: init_cycles,
            segments: Vec::new(),
            insn_counter: 0,
            bonsai_proof_id: bonsai_proof_id,
        }
    }

    /// Construct a new [Executor] from an ELF binary.
    pub fn from_elf(env: ExecutorEnv<'a>, elf: &[u8]) -> Result<Self> {
        let program = Program::load_elf(&elf, MEM_SIZE as u32)?;
        let image = MemoryImage::new(&program, PAGE_SIZE as u32)?;
        let mut bonsai_proof_id: Option<i64> = None;

        // TODO: move this to the run() function once we switch the ELF registration to
        // use MemoryImages.
        if std::env::var("BONSAI_DOGFOOD_URL").is_ok() {
            let bonsai_url = std::env::var("BONSAI_DOGFOOD_URL").unwrap();
            let proof_id = bonsai_api::register_proof(
                elf,
                bonsai_url.clone(),
                image.get_root(),
                env.input.to_owned(),
            )?;
            log::debug!(
                "session receipt: {:?}",
                bonsai_api::run_proof(bonsai_url, proof_id)
            );
            bonsai_proof_id = Some(proof_id)
        }
        Ok(Self::new_with_id(
            env,
            image,
            program.entry,
            bonsai_proof_id,
        ))
    }

    /// Run the executor until [ExitCode::Paused] or [ExitCode::Halted] is
    /// reached, producing a [Session] as a result.
    pub fn run(&mut self) -> Result<Session> {
        // Bonsai only needs the proof ID to retrieve the SessionReceipt. So a "session"
        // can be represented by a proof ID.
        if std::env::var("BONSAI_DOGFOOD_URL").is_ok() {
            log::debug!("created special session");
            return Ok(Session::new_with_proof_id(
                Vec::new(),
                Vec::new(),
                ExitCode::Halted(0),
                self.bonsai_proof_id,
            ));
        }

        self.monitor.clear_session();

        let journal = Journal::default();
        self.env
            .io
            .borrow_mut()
            .with_write_fd(fileno::JOURNAL, journal.clone());

        let mut run_loop = || -> Result<ExitCode> {
            loop {
                if let Some(exit_code) = self.step()? {
                    let total_cycles = self.total_cycles();
                    log::debug!("exit_code: {exit_code:?}, total_cycles: {total_cycles}");
                    assert!(total_cycles <= (1 << self.env.segment_limit_po2));
                    let pre_image = self.pre_image.clone();
                    self.monitor.image.hash_pages(); // TODO: hash only the dirty pages
                    let post_image_id = self.monitor.image.get_root();
                    let syscalls = take(&mut self.monitor.syscalls);
                    let faults = take(&mut self.monitor.faults);
                    self.segments.push(Segment::new(
                        pre_image,
                        post_image_id,
                        self.pre_pc,
                        faults,
                        syscalls,
                        exit_code,
                        log2_ceil(total_cycles.next_power_of_two()),
                        self.segments
                            .len()
                            .try_into()
                            .context("Too many segment to fit in u32")?,
                    ));
                    match exit_code {
                        ExitCode::SystemSplit(_) => self.split(),
                        ExitCode::SessionLimit => bail!("Session limit exceeded"),
                        ExitCode::Paused => {
                            log::debug!("Paused: {}", self.segment_cycle);
                            self.split();
                            return Ok(exit_code);
                        }
                        ExitCode::Halted(inner) => {
                            log::debug!("Halted({inner}): {}", self.segment_cycle);
                            return Ok(exit_code);
                        }
                    };
                };
            }
        };

        let exit_code = run_loop()?;
        let mut segments = Vec::new();
        std::mem::swap(&mut segments, &mut self.segments);
        Ok(Session::new(segments, journal.buf.take(), exit_code))
    }

    fn split(&mut self) {
        self.pre_image = self.monitor.image.clone();
        self.body_cycles = 0;
        self.insn_counter = 0;
        self.segment_cycle = self.init_cycles;
        self.pre_pc = self.pc;
        self.monitor.clear_segment();
    }

    /// Execute a single instruction.
    ///
    /// This can be directly used by debuggers.
    pub fn step(&mut self) -> Result<Option<ExitCode>> {
        if self.session_cycle() > self.env.get_session_limit() {
            return Ok(Some(ExitCode::SessionLimit));
        }

        let insn = self.monitor.load_u32(self.pc);
        let opcode = OpCode::decode(insn, self.pc)?;

        if let Some(op_result) = self.monitor.restore_op() {
            return Ok(self.advance(opcode, op_result));
        }

        let op_result = if opcode.major == MajorType::ECall {
            self.ecall()?
        } else {
            let registers = self.monitor.load_registers(array::from_fn(|idx| idx));
            let mut hart = HartState {
                registers,
                pc: self.pc,
                last_register_write: None,
            };

            InstructionExecutor {
                mem: &mut self.monitor,
                hart_state: &mut hart,
            }
            .step()
            .map_err(|err| anyhow!("{:?}", err))?;

            if let Some(idx) = hart.last_register_write {
                self.monitor.store_register(idx, hart.registers[idx]);
            }

            OpCodeResult::new(hart.pc, None, 0, None)
        };
        self.monitor.save_op(op_result.clone());

        if let Some(ref trace_callback) = self.env.trace_callback {
            trace_callback.borrow_mut()(TraceEvent::InstructionStart {
                cycle: self.session_cycle() as u32,
                pc: self.pc,
            })
            .unwrap();

            for event in self.monitor.trace_writes.iter() {
                trace_callback.borrow_mut()(event.clone()).unwrap();
            }
        }

        // try to execute the next instruction
        // if the segment limit is exceeded:
        // * don't increment the PC
        // * don't record any activity
        // * return ExitCode::SystemSplit
        // otherwise, commit memory and hart

        let segment_limit = self.env.get_segment_limit();
        let total_pending_cycles = self.total_pending_cycles(&opcode);
        // log::debug!(
        //     "cycle: {}, segment: {}, total: {}",
        //     self.segment_cycle,
        //     total_pending_cycles,
        //     self.total_cycles()
        // );
        let exit_code = if total_pending_cycles > segment_limit {
            Some(ExitCode::SystemSplit(self.insn_counter))
        } else {
            self.advance(opcode, op_result)
        };
        Ok(exit_code)
    }

    fn advance(&mut self, opcode: OpCode, op_result: OpCodeResult) -> Option<ExitCode> {
        log::debug!(
            "[{}] pc: 0x{:08x}, insn: 0x{:08x} => {:?}",
            self.segment_cycle,
            self.pc,
            opcode.insn,
            opcode
        );

        self.pc = op_result.pc;
        self.insn_counter += 1;
        self.body_cycles += opcode.cycles + op_result.extra_cycles;
        let total_page_read_cycles = self.monitor.total_page_read_cycles();
        // log::debug!("total_page_read_cycles: {total_page_read_cycles}");
        self.segment_cycle = self.init_cycles + total_page_read_cycles + self.body_cycles;
        self.monitor.commit(self.session_cycle());
        op_result.exit_code
    }

    fn total_cycles(&self) -> usize {
        self.init_cycles
            + self.monitor.total_fault_cycles()
            + self.body_cycles
            + self.fini_cycles
            + SHA_CYCLES
            + ZK_CYCLES
    }

    fn total_pending_cycles(&self, opcode: &OpCode) -> usize {
        // How many cycles are required for the entire segment?
        // This sum is based on:
        // - ensure we don't split in the middle of a SHA compress
        // - each page fault requires 1 PageFault cycle + CYCLES_PER_PAGE cycles
        // - leave room for fini_cycles
        // - leave room for ZK cycles
        self.init_cycles
            + self.monitor.total_pending_fault_cycles()
            + opcode.cycles
            + self.body_cycles
            + self.fini_cycles
            + SHA_CYCLES
            + ZK_CYCLES
    }

    fn session_cycle(&self) -> usize {
        self.segments.len() * self.env.get_segment_limit() + self.segment_cycle
    }

    fn ecall(&mut self) -> Result<OpCodeResult> {
        match self.monitor.load_register(REG_T0) {
            ecall::HALT => self.ecall_halt(),
            ecall::OUTPUT => self.ecall_output(),
            ecall::SOFTWARE => self.ecall_software(),
            ecall::SHA => self.ecall_sha(),
            ecall => bail!("Unknown ecall {ecall:?}"),
        }
    }

    fn ecall_halt(&mut self) -> Result<OpCodeResult> {
        let halt_type = self.monitor.load_register(REG_A0);
        match halt_type {
            halt::TERMINATE => Ok(OpCodeResult::new(
                self.pc,
                Some(ExitCode::Halted(0)),
                0,
                None,
            )),
            halt::PAUSE => Ok(OpCodeResult::new(
                self.pc + WORD_SIZE as u32,
                Some(ExitCode::Paused),
                0,
                None,
            )),
            _ => bail!("Illegal halt type: {halt_type}"),
        }
    }

    fn ecall_output(&mut self) -> Result<OpCodeResult> {
        log::debug!("ecall(output)");
        Ok(OpCodeResult::new(self.pc + WORD_SIZE as u32, None, 0, None))
    }

    fn ecall_sha(&mut self) -> Result<OpCodeResult> {
        let [out_state_ptr, in_state_ptr, mut block1_ptr, mut block2_ptr, count] = self
            .monitor
            .load_registers([REG_A0, REG_A1, REG_A2, REG_A3, REG_A4]);

        let in_state: [u8; DIGEST_BYTES] = self.monitor.load_array(in_state_ptr);
        let mut state: [u32; DIGEST_WORDS] = bytemuck::cast_slice(&in_state).try_into().unwrap();
        for word in &mut state {
            *word = word.to_be();
        }

        log::debug!("Initial sha state: {state:08x?}");
        for _ in 0..count {
            let mut block = [0u32; BLOCK_WORDS];
            for i in 0..DIGEST_WORDS {
                block[i] = self.monitor.load_u32(block1_ptr + (i * WORD_SIZE) as u32);
            }
            for i in 0..DIGEST_WORDS {
                block[DIGEST_WORDS + i] =
                    self.monitor.load_u32(block2_ptr + (i * WORD_SIZE) as u32);
            }
            log::debug!("Compressing block {block:02x?}");
            sha2::compress256(
                &mut state,
                &[*generic_array::GenericArray::from_slice(
                    bytemuck::cast_slice(&block),
                )],
            );

            block1_ptr += BLOCK_BYTES as u32;
            block2_ptr += BLOCK_BYTES as u32;
        }
        log::debug!("Final sha state: {state:08x?}");

        for word in &mut state {
            *word = u32::from_be(*word);
        }

        self.monitor
            .store_region(out_state_ptr, bytemuck::cast_slice(&state));

        Ok(OpCodeResult::new(
            self.pc + WORD_SIZE as u32,
            None,
            SHA_CYCLES * count as usize,
            None,
        ))
    }

    fn ecall_software(&mut self) -> Result<OpCodeResult> {
        let [to_guest_ptr, to_guest_words, name_ptr] =
            self.monitor.load_registers([REG_A0, REG_A1, REG_A2]);
        let syscall_name = self.monitor.load_string(name_ptr)?;
        log::debug!("Guest called syscall {syscall_name:?} requesting {to_guest_words} words back");

        let chunks = align_up(to_guest_words as usize, WORD_SIZE);
        let mut to_guest = vec![0; to_guest_words as usize];

        let handler = self
            .env
            .get_syscall(&syscall_name)
            .ok_or(anyhow!("Unknown syscall: {syscall_name:?}"))?;
        let (a0, a1) =
            handler
                .borrow_mut()
                .syscall(&syscall_name, &mut self.monitor, &mut to_guest)?;

        self.monitor
            .store_region(to_guest_ptr, bytemuck::cast_slice(&to_guest));
        self.monitor.store_register(REG_A0, a0);
        self.monitor.store_register(REG_A1, a1);

        log::debug!("Syscall returned a0: {a0:#X}, a1: {a1:#X}, chunks: {chunks}");

        // One cycle for the ecall cycle, then one for each chunk or
        // portion thereof then one to save output (a0, a1)
        Ok(OpCodeResult::new(
            self.pc + WORD_SIZE as u32,
            None,
            1 + chunks + 1,
            Some(SyscallRecord {
                to_guest,
                regs: (a0, a1),
            }),
        ))
    }
}

/// An event traced from the running VM.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub enum TraceEvent {
    /// An instruction has started at the given program counter
    InstructionStart {
        /// Cycle number since startup
        cycle: u32,
        /// Program counter of the instruction being executed
        pc: u32,
    },

    /// A register has been set
    RegisterSet {
        /// Register ID (0-16)
        reg: usize,
        /// New value in the register
        value: u32,
    },

    /// A memory location has been written
    MemorySet {
        /// Address of word that's been written
        addr: u32,
        /// Value of word that's been written
        value: u32,
    },
}

impl Debug for TraceEvent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InstructionStart { cycle, pc } => {
                write!(f, "InstructionStart({cycle}, 0x{pc:08X})")
            }
            Self::RegisterSet { reg, value } => write!(f, "RegisterSet({reg}, 0x{value:08X})"),
            Self::MemorySet { addr, value } => write!(f, "MemorySet(0x{addr:08X}, 0x{value:08X})"),
        }
    }
}
