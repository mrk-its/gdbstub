use super::prelude::*;
use crate::protocol::commands::ext::Base;

use crate::arch::{Arch, Registers};
use crate::protocol::{IdKind, SpecificIdKind, SpecificThreadId};
use crate::target::ext::base::multithread::ThreadStopReason;
use crate::target::ext::base::{BaseOps, ReplayLogPosition};
use crate::{FAKE_PID, SINGLE_THREAD_TID};

impl<T: Target, C: Connection> GdbStubImpl<T, C> {
    #[inline(always)]
    fn get_sane_any_tid(&mut self, target: &mut T) -> Result<Tid, Error<T::Error, C::Error>> {
        let tid = match target.base_ops() {
            BaseOps::SingleThread(_) => SINGLE_THREAD_TID,
            BaseOps::MultiThread(ops) => {
                let mut first_tid = None;
                ops.list_active_threads(&mut |tid| {
                    if first_tid.is_none() {
                        first_tid = Some(tid);
                    }
                })
                .map_err(Error::TargetError)?;
                // Note that `Error::NoActiveThreads` shouldn't ever occur, since this method is
                // called from the `H` packet handler, which AFAIK is only sent after the GDB
                // client has confirmed that a thread / process exists.
                //
                // If it does, that really sucks, and will require rethinking how to handle "any
                // thread" messages.
                first_tid.ok_or(Error::NoActiveThreads)?
            }
        };
        Ok(tid)
    }

    pub(crate) fn handle_base<'a>(
        &mut self,
        res: &mut ResponseWriter<C>,
        target: &mut T,
        command: Base<'a>,
    ) -> Result<HandlerStatus, Error<T::Error, C::Error>> {
        let handler_status = match command {
            // ------------------ Handshaking and Queries ------------------- //
            Base::qSupported(cmd) => {
                // XXX: actually read what the client supports, and enable/disable features
                // appropriately
                let _features = cmd.features.into_iter();

                res.write_str("PacketSize=")?;
                res.write_num(cmd.packet_buffer_len)?;

                res.write_str(";vContSupported+")?;
                res.write_str(";multiprocess+")?;
                res.write_str(";QStartNoAckMode+")?;

                let (reverse_cont, reverse_step) = match target.base_ops() {
                    BaseOps::MultiThread(ops) => (
                        ops.support_reverse_cont().is_some(),
                        ops.support_reverse_step().is_some(),
                    ),
                    BaseOps::SingleThread(ops) => (
                        ops.support_reverse_cont().is_some(),
                        ops.support_reverse_step().is_some(),
                    ),
                };

                if reverse_cont {
                    res.write_str(";ReverseContinue+")?;
                }

                if reverse_step {
                    res.write_str(";ReverseStep+")?;
                }

                if let Some(ops) = target.support_extended_mode() {
                    if ops.support_configure_aslr().is_some() {
                        res.write_str(";QDisableRandomization+")?;
                    }

                    if ops.support_configure_env().is_some() {
                        res.write_str(";QEnvironmentHexEncoded+")?;
                        res.write_str(";QEnvironmentUnset+")?;
                        res.write_str(";QEnvironmentReset+")?;
                    }

                    if ops.support_configure_startup_shell().is_some() {
                        res.write_str(";QStartupWithShell+")?;
                    }

                    if ops.support_configure_working_dir().is_some() {
                        res.write_str(";QSetWorkingDir+")?;
                    }
                }

                if let Some(ops) = target.support_breakpoints() {
                    if ops.support_sw_breakpoint().is_some() {
                        res.write_str(";swbreak+")?;
                    }

                    if ops.support_hw_breakpoint().is_some()
                        || ops.support_hw_watchpoint().is_some()
                    {
                        res.write_str(";hwbreak+")?;
                    }
                }

                if target.support_catch_syscalls().is_some() {
                    res.write_str(";QCatchSyscalls+")?;
                }

                if T::Arch::target_description_xml().is_some()
                    || target.support_target_description_xml_override().is_some()
                {
                    res.write_str(";qXfer:features:read+")?;
                }

                if target.support_memory_map().is_some() {
                    res.write_str(";qXfer:memory-map:read+")?;
                }

                if target.support_exec_file().is_some() {
                    res.write_str(";qXfer:exec-file:read+")?;
                }

                if target.support_auxv().is_some() {
                    res.write_str(";qXfer:auxv:read+")?;
                }

                HandlerStatus::Handled
            }
            Base::QStartNoAckMode(_) => {
                self.no_ack_mode = true;
                HandlerStatus::NeedsOk
            }
            Base::qXferFeaturesRead(cmd) => {
                let ret = if let Some(ops) = target.support_target_description_xml_override() {
                    ops.target_description_xml(cmd.offset, cmd.length, cmd.buf)
                        .handle_error()?
                } else if let Some(xml) = T::Arch::target_description_xml() {
                    let xml = xml.trim().as_bytes();
                    let xml_len = xml.len();

                    let start = xml_len.min(cmd.offset as usize);
                    let end = xml_len.min(cmd.offset as usize + cmd.length);

                    // LLVM isn't smart enough to realize that `end` will always be greater than
                    // `start`, and fails to elide the `slice_index_order_fail` check unless we
                    // include this seemingly useless call to `max`.
                    let data = &xml[start..end.max(start)];

                    let n = data.len().min(cmd.buf.len());
                    cmd.buf[..n].copy_from_slice(&data[..n]);
                    n
                } else {
                    // If the target hasn't provided their own XML, then the initial response to
                    // "qSupported" wouldn't have included "qXfer:features:read", and gdb wouldn't
                    // send this packet unless it was explicitly marked as supported.
                    return Err(Error::PacketUnexpected);
                };

                if ret == 0 {
                    res.write_str("l")?;
                } else {
                    res.write_str("m")?;
                    // TODO: add more specific error variant?
                    res.write_binary(cmd.buf.get(..ret).ok_or(Error::PacketBufferOverflow)?)?;
                }
                HandlerStatus::Handled
            }

            // -------------------- "Core" Functionality -------------------- //
            // TODO: Improve the '?' response based on last-sent stop reason.
            // this will be particularly relevant when working on non-stop mode.
            Base::QuestionMark(_) => {
                res.write_str("S05")?;
                HandlerStatus::Handled
            }
            Base::qAttached(cmd) => {
                let is_attached = match target.support_extended_mode() {
                    // when _not_ running in extended mode, just report that we're attaching to an
                    // existing process.
                    None => true, // assume attached to an existing process
                    // When running in extended mode, we must defer to the target
                    Some(ops) => {
                        let pid: Pid = cmd.pid.ok_or(Error::PacketUnexpected)?;
                        ops.query_if_attached(pid).handle_error()?.was_attached()
                    }
                };
                res.write_str(if is_attached { "1" } else { "0" })?;
                HandlerStatus::Handled
            }
            Base::g(_) => {
                let mut regs: <T::Arch as Arch>::Registers = Default::default();
                match target.base_ops() {
                    BaseOps::SingleThread(ops) => ops.read_registers(&mut regs),
                    BaseOps::MultiThread(ops) => {
                        ops.read_registers(&mut regs, self.current_mem_tid)
                    }
                }
                .handle_error()?;

                let mut err = Ok(());
                regs.gdb_serialize(|val| {
                    let res = match val {
                        Some(b) => res.write_hex_buf(&[b]),
                        None => res.write_str("xx"),
                    };
                    if let Err(e) = res {
                        err = Err(e);
                    }
                });
                err?;
                HandlerStatus::Handled
            }
            Base::G(cmd) => {
                let mut regs: <T::Arch as Arch>::Registers = Default::default();
                regs.gdb_deserialize(cmd.vals)
                    .map_err(|_| Error::TargetMismatch)?;

                match target.base_ops() {
                    BaseOps::SingleThread(ops) => ops.write_registers(&regs),
                    BaseOps::MultiThread(ops) => ops.write_registers(&regs, self.current_mem_tid),
                }
                .handle_error()?;

                HandlerStatus::NeedsOk
            }
            Base::m(cmd) => {
                let buf = cmd.buf;
                let addr = <T::Arch as Arch>::Usize::from_be_bytes(cmd.addr)
                    .ok_or(Error::TargetMismatch)?;

                let mut i = 0;
                let mut n = cmd.len;
                while n != 0 {
                    let chunk_size = n.min(buf.len());

                    use num_traits::NumCast;

                    let addr = addr + NumCast::from(i).ok_or(Error::TargetMismatch)?;
                    let data = &mut buf[..chunk_size];
                    match target.base_ops() {
                        BaseOps::SingleThread(ops) => ops.read_addrs(addr, data),
                        BaseOps::MultiThread(ops) => {
                            ops.read_addrs(addr, data, self.current_mem_tid)
                        }
                    }
                    .handle_error()?;

                    n -= chunk_size;
                    i += chunk_size;

                    res.write_hex_buf(data)?;
                }
                HandlerStatus::Handled
            }
            Base::M(cmd) => {
                let addr = <T::Arch as Arch>::Usize::from_be_bytes(cmd.addr)
                    .ok_or(Error::TargetMismatch)?;

                match target.base_ops() {
                    BaseOps::SingleThread(ops) => ops.write_addrs(addr, cmd.val),
                    BaseOps::MultiThread(ops) => {
                        ops.write_addrs(addr, cmd.val, self.current_mem_tid)
                    }
                }
                .handle_error()?;

                HandlerStatus::NeedsOk
            }
            Base::k(_) | Base::vKill(_) => {
                match target.support_extended_mode() {
                    // When not running in extended mode, stop the `GdbStub` and disconnect.
                    None => HandlerStatus::Disconnect(DisconnectReason::Kill),

                    // When running in extended mode, a kill command does not necessarily result in
                    // a disconnect...
                    Some(ops) => {
                        let pid = match command {
                            Base::vKill(cmd) => Some(cmd.pid),
                            _ => None,
                        };

                        let should_terminate = ops.kill(pid).handle_error()?;
                        if should_terminate.into_bool() {
                            // manually write OK, since we need to return a DisconnectReason
                            res.write_str("OK")?;
                            HandlerStatus::Disconnect(DisconnectReason::Kill)
                        } else {
                            HandlerStatus::NeedsOk
                        }
                    }
                }
            }
            Base::D(_) => {
                // TODO: plumb-through Pid when exposing full multiprocess + extended mode
                res.write_str("OK")?; // manually write OK, since we need to return a DisconnectReason
                HandlerStatus::Disconnect(DisconnectReason::Disconnect)
            }
            Base::vCont(cmd) => {
                use crate::protocol::commands::_vCont::vCont;
                match cmd {
                    vCont::Query => {
                        // Continue is part of the base protocol
                        res.write_str("vCont;c;C")?;

                        // Single stepping is optional
                        if match target.base_ops() {
                            BaseOps::SingleThread(ops) => ops.support_single_step().is_some(),
                            BaseOps::MultiThread(ops) => ops.support_single_step().is_some(),
                        } {
                            res.write_str(";s;S")?;
                        }

                        // Range stepping is optional
                        if match target.base_ops() {
                            BaseOps::SingleThread(ops) => ops.support_range_step().is_some(),
                            BaseOps::MultiThread(ops) => ops.support_range_step().is_some(),
                        } {
                            res.write_str(";r")?;
                        }

                        HandlerStatus::Handled
                    }
                    vCont::Actions(actions) => self.do_vcont(target, actions)?,
                }
            }
            // TODO?: support custom resume addr in 'c' and 's'
            //
            // vCont doesn't have a notion of "resume addr", and since the implementation of these
            // packets reuse vCont infrastructure, supporting this obscure feature will be a bit
            // annoying...
            //
            // TODO: add `support_legacy_s_c_packets` flag (similar to `use_X_packet`)
            Base::c(_) => {
                use crate::protocol::commands::_vCont::Actions;

                self.do_vcont(
                    target,
                    Actions::new_continue(SpecificThreadId {
                        pid: None,
                        tid: self.current_resume_tid,
                    }),
                )?
            }
            Base::s(_) => {
                use crate::protocol::commands::_vCont::Actions;

                self.do_vcont(
                    target,
                    Actions::new_step(SpecificThreadId {
                        pid: None,
                        tid: self.current_resume_tid,
                    }),
                )?
            }

            // ------------------- Multi-threading Support ------------------ //
            Base::H(cmd) => {
                use crate::protocol::commands::_h_upcase::Op;
                match cmd.kind {
                    Op::Other => match cmd.thread.tid {
                        IdKind::Any => self.current_mem_tid = self.get_sane_any_tid(target)?,
                        // "All" threads doesn't make sense for memory accesses
                        IdKind::All => return Err(Error::PacketUnexpected),
                        IdKind::WithId(tid) => self.current_mem_tid = tid,
                    },
                    // technically, this variant is deprecated in favor of vCont...
                    Op::StepContinue => match cmd.thread.tid {
                        IdKind::Any => {
                            self.current_resume_tid =
                                SpecificIdKind::WithId(self.get_sane_any_tid(target)?)
                        }
                        IdKind::All => self.current_resume_tid = SpecificIdKind::All,
                        IdKind::WithId(tid) => {
                            self.current_resume_tid = SpecificIdKind::WithId(tid)
                        }
                    },
                }
                HandlerStatus::NeedsOk
            }
            Base::qfThreadInfo(_) => {
                res.write_str("m")?;

                match target.base_ops() {
                    BaseOps::SingleThread(_) => res.write_specific_thread_id(SpecificThreadId {
                        pid: Some(SpecificIdKind::WithId(FAKE_PID)),
                        tid: SpecificIdKind::WithId(SINGLE_THREAD_TID),
                    })?,
                    BaseOps::MultiThread(ops) => {
                        let mut err: Result<_, Error<T::Error, C::Error>> = Ok(());
                        let mut first = true;
                        ops.list_active_threads(&mut |tid| {
                            // TODO: replace this with a try block (once stabilized)
                            let e = (|| {
                                if !first {
                                    res.write_str(",")?
                                }
                                first = false;
                                res.write_specific_thread_id(SpecificThreadId {
                                    pid: Some(SpecificIdKind::WithId(FAKE_PID)),
                                    tid: SpecificIdKind::WithId(tid),
                                })?;
                                Ok(())
                            })();

                            if let Err(e) = e {
                                err = Err(e)
                            }
                        })
                        .map_err(Error::TargetError)?;
                        err?;
                    }
                }

                HandlerStatus::Handled
            }
            Base::qsThreadInfo(_) => {
                res.write_str("l")?;
                HandlerStatus::Handled
            }
            Base::T(cmd) => {
                let alive = match cmd.thread.tid {
                    IdKind::WithId(tid) => match target.base_ops() {
                        BaseOps::SingleThread(_) => tid == SINGLE_THREAD_TID,
                        BaseOps::MultiThread(ops) => {
                            ops.is_thread_alive(tid).map_err(Error::TargetError)?
                        }
                    },
                    // TODO: double-check if GDB ever sends other variants
                    // Even after ample testing, this arm has never been hit...
                    _ => return Err(Error::PacketUnexpected),
                };
                if alive {
                    HandlerStatus::NeedsOk
                } else {
                    // any error code will do
                    return Err(Error::NonFatalError(1));
                }
            }
        };
        Ok(handler_status)
    }

    fn do_vcont_single_thread(
        ops: &mut dyn crate::target::ext::base::singlethread::SingleThreadOps<
            Arch = T::Arch,
            Error = T::Error,
        >,
        actions: &crate::protocol::commands::_vCont::Actions,
    ) -> Result<(), Error<T::Error, C::Error>> {
        use crate::protocol::commands::_vCont::VContKind;

        let mut actions = actions.iter();
        let first_action = actions
            .next()
            .ok_or(Error::PacketParse(
                crate::protocol::PacketParseError::MalformedCommand,
            ))?
            .ok_or(Error::PacketParse(
                crate::protocol::PacketParseError::MalformedCommand,
            ))?;

        let invalid_second_action = match actions.next() {
            None => false,
            Some(act) => match act {
                None => {
                    return Err(Error::PacketParse(
                        crate::protocol::PacketParseError::MalformedCommand,
                    ))
                }
                Some(act) => !matches!(act.kind, VContKind::Continue),
            },
        };

        if invalid_second_action || actions.next().is_some() {
            return Err(Error::PacketUnexpected);
        }

        match first_action.kind {
            VContKind::Continue | VContKind::ContinueWithSig(_) => {
                let signal = match first_action.kind {
                    VContKind::ContinueWithSig(sig) => Some(sig),
                    _ => None,
                };

                ops.resume(signal).map_err(Error::TargetError)?;
                Ok(())
            }
            VContKind::Step | VContKind::StepWithSig(_) if ops.support_single_step().is_some() => {
                let ops = ops.support_single_step().unwrap();

                let signal = match first_action.kind {
                    VContKind::StepWithSig(sig) => Some(sig),
                    _ => None,
                };

                ops.step(signal).map_err(Error::TargetError)?;
                Ok(())
            }
            VContKind::RangeStep(start, end) if ops.support_range_step().is_some() => {
                let ops = ops.support_range_step().unwrap();

                let start = start.decode().map_err(|_| Error::TargetMismatch)?;
                let end = end.decode().map_err(|_| Error::TargetMismatch)?;

                ops.resume_range_step(start, end)
                    .map_err(Error::TargetError)?;
                Ok(())
            }
            // TODO: update this case when non-stop mode is implemented
            VContKind::Stop => Err(Error::PacketUnexpected),

            // Instead of using `_ =>`, explicitly list out any remaining unguarded cases.
            VContKind::RangeStep(..) | VContKind::Step | VContKind::StepWithSig(..) => {
                Err(Error::PacketUnexpected)
            }
        }
    }

    fn do_vcont_multi_thread(
        ops: &mut dyn crate::target::ext::base::multithread::MultiThreadOps<
            Arch = T::Arch,
            Error = T::Error,
        >,
        actions: &crate::protocol::commands::_vCont::Actions,
    ) -> Result<(), Error<T::Error, C::Error>> {
        ops.clear_resume_actions().map_err(Error::TargetError)?;

        for action in actions.iter() {
            use crate::protocol::commands::_vCont::VContKind;

            let action = action.ok_or(Error::PacketParse(
                crate::protocol::PacketParseError::MalformedCommand,
            ))?;

            match action.kind {
                VContKind::Continue | VContKind::ContinueWithSig(_) => {
                    let signal = match action.kind {
                        VContKind::ContinueWithSig(sig) => Some(sig),
                        _ => None,
                    };

                    match action.thread.map(|thread| thread.tid) {
                        // An action with no thread-id matches all threads
                        None | Some(SpecificIdKind::All) => {
                            // Target API contract specifies that the default
                            // resume action for all threads is continue.
                        }
                        Some(SpecificIdKind::WithId(tid)) => ops
                            .set_resume_action_continue(tid, signal)
                            .map_err(Error::TargetError)?,
                    }
                }
                VContKind::Step | VContKind::StepWithSig(_)
                    if ops.support_single_step().is_some() =>
                {
                    let ops = ops.support_single_step().unwrap();

                    let signal = match action.kind {
                        VContKind::StepWithSig(sig) => Some(sig),
                        _ => None,
                    };

                    match action.thread.map(|thread| thread.tid) {
                        // An action with no thread-id matches all threads
                        None | Some(SpecificIdKind::All) => {
                            error!("GDB client sent 'step' as default resume action");
                            return Err(Error::PacketUnexpected);
                        }
                        Some(SpecificIdKind::WithId(tid)) => {
                            ops.set_resume_action_step(tid, signal)
                                .map_err(Error::TargetError)?;
                        }
                    };
                }

                VContKind::RangeStep(start, end) if ops.support_range_step().is_some() => {
                    let ops = ops.support_range_step().unwrap();

                    match action.thread.map(|thread| thread.tid) {
                        // An action with no thread-id matches all threads
                        None | Some(SpecificIdKind::All) => {
                            error!("GDB client sent 'range step' as default resume action");
                            return Err(Error::PacketUnexpected);
                        }
                        Some(SpecificIdKind::WithId(tid)) => {
                            let start = start.decode().map_err(|_| Error::TargetMismatch)?;
                            let end = end.decode().map_err(|_| Error::TargetMismatch)?;

                            ops.set_resume_action_range_step(tid, start, end)
                                .map_err(Error::TargetError)?;
                        }
                    };
                }
                // TODO: update this case when non-stop mode is implemented
                VContKind::Stop => return Err(Error::PacketUnexpected),

                // Instead of using `_ =>`, explicitly list out any remaining unguarded cases.
                VContKind::RangeStep(..) | VContKind::Step | VContKind::StepWithSig(..) => {
                    return Err(Error::PacketUnexpected)
                }
            }
        }

        ops.resume().map_err(Error::TargetError)
    }

    fn do_vcont(
        &mut self,
        target: &mut T,
        actions: crate::protocol::commands::_vCont::Actions,
    ) -> Result<HandlerStatus, Error<T::Error, C::Error>> {
        match target.base_ops() {
            BaseOps::SingleThread(ops) => Self::do_vcont_single_thread(ops, &actions)?,
            BaseOps::MultiThread(ops) => Self::do_vcont_multi_thread(ops, &actions)?,
        };

        Ok(HandlerStatus::DeferredStopReason)
    }

    fn write_break_common(
        &mut self,
        res: &mut ResponseWriter<C>,
        tid: Tid,
    ) -> Result<(), Error<T::Error, C::Error>> {
        self.current_mem_tid = tid;
        self.current_resume_tid = SpecificIdKind::WithId(tid);

        res.write_str("T05")?;

        res.write_str("thread:")?;
        res.write_specific_thread_id(SpecificThreadId {
            pid: Some(SpecificIdKind::WithId(FAKE_PID)),
            tid: SpecificIdKind::WithId(tid),
        })?;
        res.write_str(";")?;

        Ok(())
    }

    pub(crate) fn finish_exec(
        &mut self,
        res: &mut ResponseWriter<C>,
        target: &mut T,
        stop_reason: ThreadStopReason<<T::Arch as Arch>::Usize>,
    ) -> Result<FinishExecStatus, Error<T::Error, C::Error>> {
        macro_rules! guard_reverse_exec {
            () => {{
                let (reverse_cont, reverse_step) = match target.base_ops() {
                    BaseOps::MultiThread(ops) => (
                        ops.support_reverse_cont().is_some(),
                        ops.support_reverse_step().is_some(),
                    ),
                    BaseOps::SingleThread(ops) => (
                        ops.support_reverse_cont().is_some(),
                        ops.support_reverse_step().is_some(),
                    ),
                };
                reverse_cont || reverse_step
            }};
        }

        macro_rules! guard_break {
            ($op:ident) => {
                target
                    .support_breakpoints()
                    .and_then(|ops| ops.$op())
                    .is_some()
            };
        }

        macro_rules! guard_catch_syscall {
            () => {
                target.support_catch_syscalls().is_some()
            };
        }

        let status = match stop_reason {
            ThreadStopReason::DoneStep => {
                res.write_str("S05")?;
                FinishExecStatus::Handled
            }
            ThreadStopReason::Signal(sig) => {
                res.write_str("S")?;
                res.write_num(sig as u8)?;
                FinishExecStatus::Handled
            }
            ThreadStopReason::Exited(code) => {
                res.write_str("W")?;
                res.write_num(code)?;
                FinishExecStatus::Disconnect(DisconnectReason::TargetExited(code))
            }
            ThreadStopReason::Terminated(sig) => {
                res.write_str("X")?;
                res.write_num(sig as u8)?;
                FinishExecStatus::Disconnect(DisconnectReason::TargetTerminated(sig))
            }
            ThreadStopReason::SwBreak(tid) if guard_break!(support_sw_breakpoint) => {
                crate::__dead_code_marker!("sw_breakpoint", "stop_reason");

                self.write_break_common(res, tid)?;
                res.write_str("swbreak:;")?;
                FinishExecStatus::Handled
            }
            ThreadStopReason::HwBreak(tid) if guard_break!(support_hw_breakpoint) => {
                crate::__dead_code_marker!("hw_breakpoint", "stop_reason");

                self.write_break_common(res, tid)?;
                res.write_str("hwbreak:;")?;
                FinishExecStatus::Handled
            }
            ThreadStopReason::Watch { tid, kind, addr } if guard_break!(support_hw_watchpoint) => {
                crate::__dead_code_marker!("hw_watchpoint", "stop_reason");

                self.write_break_common(res, tid)?;

                use crate::target::ext::breakpoints::WatchKind;
                match kind {
                    WatchKind::Write => res.write_str("watch:")?,
                    WatchKind::Read => res.write_str("rwatch:")?,
                    WatchKind::ReadWrite => res.write_str("awatch:")?,
                }
                res.write_num(addr)?;
                res.write_str(";")?;
                FinishExecStatus::Handled
            }
            ThreadStopReason::ReplayLog(pos) if guard_reverse_exec!() => {
                crate::__dead_code_marker!("reverse_exec", "stop_reason");

                res.write_str("T05")?;

                res.write_str("replaylog:")?;
                res.write_str(match pos {
                    ReplayLogPosition::Begin => "begin",
                    ReplayLogPosition::End => "end",
                })?;
                res.write_str(";")?;

                FinishExecStatus::Handled
            }
            ThreadStopReason::CatchSyscall { number, position } if guard_catch_syscall!() => {
                crate::__dead_code_marker!("catch_syscall", "stop_reason");

                res.write_str("T05")?;

                use crate::target::ext::catch_syscalls::CatchSyscallPosition;
                res.write_str(match position {
                    CatchSyscallPosition::Entry => "syscall_entry:",
                    CatchSyscallPosition::Return => "syscall_return:",
                })?;
                res.write_num(number)?;
                res.write_str(";")?;

                FinishExecStatus::Handled
            }
            // Explicitly avoid using `_ =>` to handle the "unguarded" variants, as doing so would
            // squelch the useful compiler error that crops up whenever stop reasons are added.
            ThreadStopReason::SwBreak(_)
            | ThreadStopReason::HwBreak(_)
            | ThreadStopReason::Watch { .. }
            | ThreadStopReason::ReplayLog(_)
            | ThreadStopReason::CatchSyscall { .. } => {
                return Err(Error::UnsupportedStopReason);
            }
        };

        Ok(status)
    }
}

pub(crate) enum FinishExecStatus {
    Handled,
    Disconnect(DisconnectReason),
}
