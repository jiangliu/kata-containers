// Copyright (c) 2019 Ant Financial
//
// SPDX-License-Identifier: Apache-2.0
//

use futures::*;
use grpcio::{EnvBuilder, Server, ServerBuilder};
use grpcio::{RpcStatus, RpcStatusCode};

use protobuf::{RepeatedField, SingularPtrField};
use protocols::agent::CopyFileRequest;
use protocols::agent::{
    AgentDetails, GuestDetailsResponse, ListProcessesResponse, ReadStreamResponse,
    WaitProcessResponse, WriteStreamResponse,
};
use protocols::empty::Empty;
use protocols::health::{HealthCheckResponse, HealthCheckResponse_ServingStatus};
use protocols::oci::{LinuxNamespace, Spec};
use rustjail;
use rustjail::container::{BaseContainer, LinuxContainer};
use rustjail::errors::*;
use rustjail::process::Process;
use rustjail::process::ProcessOperations;
use rustjail::specconv::CreateOpts;

use nix::errno::Errno;
use nix::sys::signal::Signal;
use nix::sys::stat;
use nix::unistd::{self, Gid, Pid, Uid};

use crate::device::{add_devices, rescan_pci_bus};
use crate::linux_abi::*;
use crate::mount::{add_storages, remove_mounts, STORAGEHANDLERLIST};
use crate::namespace::{NSTYPEIPC, NSTYPEPID, NSTYPEUTS};
use crate::netlink::{RtnlHandle, NETLINK_ROUTE};
use crate::random;
use crate::sandbox::Sandbox;
use crate::version::{AGENT_VERSION, API_VERSION};

use libc::{self, c_ushort, pid_t, winsize, TIOCSWINSZ};
use serde_json;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::FileExt;
use std::os::unix::io::RawFd;
use std::os::unix::prelude::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const CONTAINER_BASE: &str = "/run/kata-containers";

// Convenience macro to obtain the scope logger
macro_rules! sl {
    () => {
        slog_scope::logger()
    };
}

#[derive(Clone)]
struct agentService {
    sandbox: Arc<Mutex<Sandbox>>,
    test: u32,
}

impl agentService {
    fn do_create_container(&mut self, req: protocols::agent::CreateContainerRequest) -> Result<()> {
        info!(
            sl!(),
            "create container";
            "container-id" => &req.container_id,
            "exec-id" => &req.exec_id,
        );

        let mut oci_spec = req.OCI.clone();
        let oci = match oci_spec.as_mut() {
            Some(v) => v,
            None => return error_code(Errno::EINVAL),
        };
        if oci.Process.is_none() {
            info!(sl!(), "no process configurations!");
            return error_code(Errno::EINVAL);
        }

        // re-scan PCI bus, looking for hidden devices
        rescan_pci_bus().chain_err(|| "Could not rescan PCI bus")?;

        // Some devices need some extra processing (the ones invoked with
        // --device for instance), and that's what this call is doing. It
        // updates the devices listed in the OCI spec, so that they actually
        // match real devices inside the VM. This step is necessary since we
        // cannot predict everything from the caller.
        add_devices(&req.devices.to_vec(), oci, &self.sandbox)?;

        // Both rootfs and volumes (invoked with --volume for instance) will
        // be processed the same way. The idea is to always mount any provided
        // storage to the specified MountPoint, so that it will match what's
        // inside oci.Mounts.
        // After all those storages have been processed, no matter the order
        // here, the agent will rely on rustjail (using the oci.Mounts
        // list) to bind mount all of them inside the container.
        let m = add_storages(sl!(), req.storages.to_vec(), &self.sandbox)?;
        // safe to unwrap() because we don't expect poisoned lock here.
        let mut s = self.sandbox.lock().unwrap();
        s.container_mounts.insert(req.container_id.clone(), m);

        update_container_namespaces(&s, oci)?;

        // write spec to bundle path, hooks might read ocispec
        setup_bundle(oci)?;

        let opts = CreateOpts {
            cgroup_name: String::new(),
            use_systemd_cgroup: false,
            no_pivot_root: s.no_pivot_root,
            no_new_keyring: false,
            spec: Some(oci.clone()),
            rootless_euid: false,
            rootless_cgroup: false,
        };

        let mut ctr: LinuxContainer =
            LinuxContainer::new(req.container_id.as_str(), CONTAINER_BASE, opts, &sl!())?;
        let p = Process::new(&sl!(), oci.get_Process(), &req.exec_id, true)?;
        ctr.start(p)?;
        s.add_container(ctr);
        info!(sl!(), "created container!");

        Ok(())
    }

    fn do_start_container(&mut self, req: protocols::agent::StartContainerRequest) -> Result<()> {
        // safe to unwrap() because we don't expect poisoned lock here.
        let mut s = self.sandbox.lock().unwrap();
        match s.get_container(&req.container_id) {
            Some(ctr) => ctr.exec(),
            None => error_code(Errno::EINVAL),
        }
    }

    fn do_remove_container(&mut self, req: protocols::agent::RemoveContainerRequest) -> Result<()> {
        let mut sandbox;
        let mut cmounts: Vec<String> = vec![];

        if req.timeout == 0 {
            // safe to unwrap() because we don't expect poisoned lock here.
            sandbox = self.sandbox.lock().unwrap();
            if let Some(ctr) = sandbox.get_container(&req.container_id) {
                ctr.destroy()?;
            } else {
                return error_code(Errno::EINVAL);
            }
        } else {
            // timeout != 0
            let s = self.sandbox.clone();
            let cid2 = req.container_id.clone();
            let (tx, rx) = mpsc::channel();

            let handle = thread::spawn(move || {
                // safe to unwrap() because we don't expect poisoned lock here.
                let mut sandbox = s.lock().unwrap();
                if let Some(ctr) = sandbox.get_container(&cid2) {
                    match ctr.destroy() {
                        Ok(_) => tx.send(0),
                        // TODO: handle error code
                        Err(_) => tx.send(Errno::UnknownErrno as i32),
                    }
                } else {
                    tx.send(Errno::EINVAL as i32)
                }
            });

            // TODO: the timeout doesn't make sense, join() will always wait for the worker thread
            // to exit.
            if let Ok(val) = rx.recv_timeout(Duration::from_secs(req.timeout as u64)) {
                if val != 0 {
                    return error_code(Errno::from_i32(val));
                }
            } else {
                return error_code(Errno::ETIME);
            }
            if handle.join().is_err() {
                return error_code(Errno::UnknownErrno);
            }

            // safe to unwrap() because we don't expect poisoned lock here.
            sandbox = self.sandbox.lock().unwrap();
        }

        // Find the sandbox storage used by this container
        if let Some(mounts) = sandbox.container_mounts.get(&req.container_id) {
            remove_mounts(&mounts)?;
            for m in mounts.iter() {
                if sandbox.storages.get(m).is_some() {
                    cmounts.push(m.to_string());
                }
            }
        }
        for m in cmounts.iter() {
            sandbox.unset_and_remove_sandbox_storage(m)?;
        }
        sandbox.container_mounts.remove(&req.container_id);

        sandbox.containers.remove(&req.container_id);

        Ok(())
    }

    fn do_exec_process(&mut self, req: protocols::agent::ExecProcessRequest) -> Result<()> {
        info!(
            sl!(),
            "exec process";
            "container-id" => &req.container_id,
            "exec-id" => &req.exec_id,
        );

        // safe to unwrap() because we don't expect poisoned lock here.
        let mut sandbox = self.sandbox.lock().unwrap();
        if let Some(ocip) = req.process.as_ref() {
            let p = Process::new(&sl!(), ocip, &req.exec_id, false)?;
            if let Some(ctr) = sandbox.get_container(&req.container_id) {
                return ctr.run(p);
            }
        }

        error_code(Errno::EINVAL)
    }

    fn do_signal_process(&mut self, req: protocols::agent::SignalProcessRequest) -> Result<()> {
        info!(
            sl!(),
            "signal process";
            "container-id" => &req.container_id,
            "exec-id" => &req.exec_id,
        );

        // safe to unwrap() because we don't expect poisoned lock here.
        let mut sandbox = self.sandbox.lock().unwrap();
        let p = find_process(&mut sandbox, &req.container_id, &req.exec_id, true)?;

        if let Ok(mut signal) = Signal::from_c_int(req.signal as i32) {
            // For container initProcess, if it hasn't installed handler for "SIGTERM" signal,
            // it will ignore the "SIGTERM" signal sent to it, thus send it "SIGKILL" signal
            // instead of "SIGTERM" to terminate it.
            if p.init && signal == Signal::SIGTERM && !is_signal_handled(p.pid, req.signal) {
                signal = Signal::SIGKILL;
            }
            p.signal(signal)?;
            Ok(())
        } else {
            error_code(Errno::EINVAL)
        }
    }

    fn do_wait_process(
        &mut self,
        req: protocols::agent::WaitProcessRequest,
    ) -> Result<protocols::agent::WaitProcessResponse> {
        info!(
            sl!(),
            "wait process";
            "container-id" => &req.container_id,
            "exec-id" => &req.exec_id,
        );

        // safe to unwrap() because we don't expect poisoned lock here.
        let mut sandbox = self.sandbox.lock().unwrap();
        let p = find_process(&mut sandbox, &req.container_id, &req.exec_id, false)?;
        let exit_pipe_r: RawFd = match p.exit_pipe_r.as_ref() {
            Some(v) => *v,
            None => -1,
        };
        let pid = p.pid;
        drop(sandbox);

        if exit_pipe_r != -1 {
            info!(sl!(), "reading exit pipe");
            let mut buf: Vec<u8> = vec![0, 1];
            let _ = unistd::read(exit_pipe_r, buf.as_mut_slice());
        }

        let mut resp = WaitProcessResponse::new();
        // safe to unwrap() because we don't expect poisoned lock here.
        let mut sandbox = self.sandbox.lock().unwrap();
        if let Some(ctr) = sandbox.get_container(&req.container_id) {
            // TODO: is it safe to lookup the process by the cached pid?
            if let Some(p) = ctr.processes.get_mut(&pid) {
                // Close all fds
                if let Some(fd) = p.parent_stdin.take() {
                    let _ = unistd::close(fd);
                }
                if let Some(fd) = p.parent_stdout.take() {
                    let _ = unistd::close(fd);
                }
                if let Some(fd) = p.parent_stderr.take() {
                    let _ = unistd::close(fd);
                }
                if let Some(fd) = p.term_master.take() {
                    let _ = unistd::close(fd);
                }
                if let Some(fd) = p.exit_pipe_r.take() {
                    let _ = unistd::close(fd);
                }
                resp.status = p.exit_code;
                ctr.processes.remove(&pid);
            }
        }

        Ok(resp)
    }

    fn do_write_stream(
        &mut self,
        req: protocols::agent::WriteStreamRequest,
    ) -> Result<protocols::agent::WriteStreamResponse> {
        info!(
            sl!(),
            "write stdin";
            "container-id" => &req.container_id,
            "exec-id" => &req.exec_id,
        );

        // safe to unwrap() because we don't expect poisoned lock here.
        let mut sandbox = self.sandbox.lock().unwrap();
        let p = find_process(&mut sandbox, &req.container_id, &req.exec_id, false)?;
        let fd_option = if p.term_master.is_some() {
            // use ptmx io
            p.term_master.as_ref()
        } else {
            // use piped io
            p.parent_stdin.as_ref()
        };
        let fd = match fd_option {
            Some(v) => *v,
            None => {
                return Err(ErrorKind::Nix(nix::Error::from_errno(nix::errno::Errno::EIO)).into())
            }
        };
        drop(sandbox);

        let mut l = req.data.len();
        match unistd::write(fd, req.data.as_slice()) {
            Ok(v) => {
                if v < l {
                    /*
                    let f = sink.fail(RpcStatus::new(
                        RpcStatusCode::InvalidArgument,
                        Some(format!("write error"))))
                    .map_err(|_e| error!(sl!(), "write error"));
                    ctx.spawn(f);
                    return;
                    */
                    info!(sl!(), "write {} bytes", v);
                    l = v;
                }
            }
            Err(e) => match e {
                nix::Error::Sys(nix::errno::Errno::EAGAIN) => l = 0,
                _ => {
                    return Err(
                        ErrorKind::Nix(nix::Error::from_errno(nix::errno::Errno::EIO)).into(),
                    );
                }
            },
        }

        let mut resp = WriteStreamResponse::new();
        resp.set_len(l as u32);

        Ok(resp)
    }

    fn do_read_stream(
        &mut self,
        req: protocols::agent::ReadStreamRequest,
        stdout: bool,
    ) -> Result<protocols::agent::ReadStreamResponse> {
        info!(
            sl!(),
            "read stream";
            "container-id" => &req.container_id,
            "exec-id" => &req.exec_id,
        );

        // safe to unwrap() because we don't expect poisoned lock here.
        let mut sandbox = self.sandbox.lock().unwrap();
        let p = find_process(&mut sandbox, &req.container_id, &req.exec_id, false)?;
        let fd_option = {
            if p.term_master.is_some() {
                p.term_master.as_ref()
            } else if stdout {
                p.parent_stdout.as_ref()
            } else {
                p.parent_stderr.as_ref()
            }
        };
        let fd = match fd_option {
            Some(v) => *v,
            None => return error_code(Errno::EINVAL),
        };
        drop(sandbox);

        let vector = read_stream(fd, req.len as usize)?;
        let mut resp = ReadStreamResponse::new();
        resp.set_data(vector);

        Ok(resp)
    }
}

impl protocols::agent_grpc::AgentService for agentService {
    fn create_container(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::CreateContainerRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        if self.do_create_container(req).is_err() {
            let f = sink
                .fail(RpcStatus::new(
                    RpcStatusCode::Internal,
                    Some("fail to create container".to_string()),
                ))
                .map_err(|_e| error!(sl!(), "container create fail"));
            ctx.spawn(f);
        } else {
            let resp = Empty::new();
            let f = sink
                .success(resp)
                .map_err(move |_e| error!(sl!(), "fail to create container"));
            ctx.spawn(f);
        }
    }

    fn start_container(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::StartContainerRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        if self.do_start_container(req).is_err() {
            let f = sink
                .fail(RpcStatus::new(
                    RpcStatusCode::Internal,
                    Some("fail to find container".to_string()),
                ))
                .map_err(move |_e| error!(sl!(), "get container fail"));
            ctx.spawn(f);
            return;
        }

        info!(sl!(), "exec process!\n");

        let resp = Empty::new();
        let f = sink
            .success(resp)
            .map_err(move |_e| error!(sl!(), "fail to create container"));
        ctx.spawn(f);
    }

    fn remove_container(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::RemoveContainerRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        if self.do_remove_container(req).is_err() {
            let f = sink
                .fail(RpcStatus::new(
                    RpcStatusCode::Internal,
                    Some(String::from("fail to remove container")),
                ))
                .map_err(move |_e| error!(sl!(), "remove container failed"));
            ctx.spawn(f);
        } else {
            let resp = Empty::new();
            let f = sink
                .success(resp)
                .map_err(|_e| error!(sl!(), "cannot destroy container"));
            ctx.spawn(f);
        }
    }
    fn exec_process(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::ExecProcessRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        if self.do_exec_process(req).is_err() {
            let f = sink
                .fail(RpcStatus::new(
                    RpcStatusCode::Internal,
                    Some(String::from("fail to exec process!")),
                ))
                .map_err(|_e| error!(sl!(), "fail to exec process!"));
            ctx.spawn(f);
        } else {
            let resp = Empty::new();
            let f = sink
                .success(resp)
                .map_err(move |_e| error!(sl!(), "cannot exec process"));
            ctx.spawn(f);
        }
    }
    fn signal_process(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::SignalProcessRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        if self.do_signal_process(req).is_err() {
            let f = sink
                .fail(RpcStatus::new(
                    RpcStatusCode::Internal,
                    Some(String::from("fail to signal process!")),
                ))
                .map_err(|_e| error!(sl!(), "fail to signal process!"));
            ctx.spawn(f);
        } else {
            let resp = Empty::new();
            let f = sink
                .success(resp)
                .map_err(|_e| error!(sl!(), "cannot signal process"));
            ctx.spawn(f);
        }
    }
    fn wait_process(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::WaitProcessRequest,
        sink: ::grpcio::UnarySink<protocols::agent::WaitProcessResponse>,
    ) {
        if let Ok(resp) = self.do_wait_process(req) {
            let f = sink
                .success(resp)
                .map_err(|_e| error!(sl!(), "cannot wait process"));
            ctx.spawn(f);
        } else {
            let f = sink
                .fail(RpcStatus::new(
                    RpcStatusCode::Internal,
                    Some(String::from("fail to wait process!")),
                ))
                .map_err(|_e| error!(sl!(), "fail to wait process!"));
            ctx.spawn(f);
        }
    }
    fn list_processes(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::ListProcessesRequest,
        sink: ::grpcio::UnarySink<protocols::agent::ListProcessesResponse>,
    ) {
        let cid = req.container_id.clone();
        let format = req.format.clone();
        let mut args = req.args.clone().into_vec();
        let mut resp = ListProcessesResponse::new();

        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().unwrap();

        let ctr = sandbox.get_container(cid.as_str()).unwrap();
        let pids = ctr.processes().unwrap();

        match format.as_str() {
            "table" => {}
            "json" => {
                resp.process_list = serde_json::to_vec(&pids).unwrap();
                let f = sink
                    .success(resp)
                    .map_err(|_e| error!(sl!(), "cannot handle json resp"));
                ctx.spawn(f);
                return;
            }
            _ => {
                let f = sink
                    .fail(RpcStatus::new(
                        RpcStatusCode::InvalidArgument,
                        Some(String::from("invalid format")),
                    ))
                    .map_err(|_e| error!(sl!(), "invalid format!"));
                ctx.spawn(f);
                return;
            }
        }

        // format "table"
        if args.is_empty() {
            // default argument
            args = vec!["-ef".to_string()];
        }

        let output = Command::new("ps")
            .args(args.as_slice())
            .stdout(Stdio::piped())
            .output()
            .expect("ps failed");

        let out: String = String::from_utf8(output.stdout).unwrap();
        let mut lines: Vec<String> = out.split('\n').map(|v| v.to_string()).collect();

        let predicate = |v| v == "PID";

        let pid_index = lines[0].split_whitespace().position(predicate).unwrap();

        let mut result = String::new();
        result.push_str(lines[0].as_str());

        lines.remove(0);
        for line in &lines {
            if line.trim().is_empty() {
                continue;
            }

            let fields: Vec<String> = line.split_whitespace().map(|v| v.to_string()).collect();

            if fields.len() < pid_index + 1 {
                warn!(sl!(), "corrupted output?");
                continue;
            }
            let pid = fields[pid_index].trim().parse::<i32>().unwrap();

            for p in &pids {
                if pid == *p {
                    result.push_str(line.as_str());
                }
            }
        }

        resp.process_list = Vec::from(result);

        let f = sink
            .success(resp)
            .map_err(|_e| error!(sl!(), "list processes failed"));
        ctx.spawn(f);
    }
    fn update_container(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::UpdateContainerRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        let cid = req.container_id.clone();
        let res = req.resources.clone();

        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().unwrap();

        let ctr = sandbox.get_container(cid.as_str()).unwrap();

        let resp = Empty::new();

        if res.is_some() {
            match ctr.set(res.unwrap()) {
                Err(_e) => {
                    let f = sink
                        .fail(RpcStatus::new(
                            RpcStatusCode::Internal,
                            Some("internal error".to_string()),
                        ))
                        .map_err(|_e| error!(sl!(), "internal error!"));
                    ctx.spawn(f);
                    return;
                }

                Ok(()) => {}
            }
        }

        let f = sink
            .success(resp)
            .map_err(|_e| error!(sl!(), "update container failed!"));

        ctx.spawn(f);
    }
    fn stats_container(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::StatsContainerRequest,
        sink: ::grpcio::UnarySink<protocols::agent::StatsContainerResponse>,
    ) {
        let cid = req.container_id.clone();
        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().unwrap();

        let ctr = sandbox.get_container(cid.as_str()).unwrap();

        let resp = match ctr.stats() {
            Err(_e) => {
                let f = sink
                    .fail(RpcStatus::new(
                        RpcStatusCode::Internal,
                        Some("internal error!".to_string()),
                    ))
                    .map_err(|_e| error!(sl!(), "internal error!"));
                ctx.spawn(f);
                return;
            }

            Ok(r) => r,
        };

        let f = sink
            .success(resp)
            .map_err(|_e| error!(sl!(), "stats containers failed!"));
        ctx.spawn(f);
    }
    fn pause_container(
        &mut self,
        _ctx: ::grpcio::RpcContext,
        _req: protocols::agent::PauseContainerRequest,
        _sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
    }
    fn resume_container(
        &mut self,
        _ctx: ::grpcio::RpcContext,
        _req: protocols::agent::ResumeContainerRequest,
        _sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
    }
    fn write_stdin(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::WriteStreamRequest,
        sink: ::grpcio::UnarySink<protocols::agent::WriteStreamResponse>,
    ) {
        if let Ok(resp) = self.do_write_stream(req) {
            let f = sink
                .success(resp)
                .map_err(|_e| error!(sl!(), "writestream request failed!"));

            ctx.spawn(f);
        } else {
            let f = sink
                .fail(RpcStatus::new(
                    RpcStatusCode::InvalidArgument,
                    Some(String::from("write stream failed")),
                ))
                .map_err(move |_e| error!(sl!(), "write stream failed"));
            ctx.spawn(f);
        }
    }
    fn read_stdout(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::ReadStreamRequest,
        sink: ::grpcio::UnarySink<protocols::agent::ReadStreamResponse>,
    ) {
        if let Ok(resp) = self.do_read_stream(req, true) {
            let f = sink
                .success(resp)
                .map_err(move |_e| error!(sl!(), "read stdout error!"));

            ctx.spawn(f);
        } else {
            let f = sink
                .fail(RpcStatus::new(
                    RpcStatusCode::Internal,
                    Some(String::from("failed to read stdout")),
                ))
                .map_err(move |_e| error!(sl!(), "read stdout failed"));
            ctx.spawn(f);
        }
    }
    fn read_stderr(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::ReadStreamRequest,
        sink: ::grpcio::UnarySink<protocols::agent::ReadStreamResponse>,
    ) {
        if let Ok(resp) = self.do_read_stream(req, false) {
            let f = sink
                .success(resp)
                .map_err(move |_e| error!(sl!(), "read stderr error!"));

            ctx.spawn(f);
        } else {
            let f = sink
                .fail(RpcStatus::new(
                    RpcStatusCode::Internal,
                    Some(String::from("failed to read stderr")),
                ))
                .map_err(move |_e| error!(sl!(), "read stderr failed"));
            ctx.spawn(f);
        }
    }
    fn close_stdin(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::CloseStdinRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        let cid = req.container_id.clone();
        let eid = req.exec_id.clone();
        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().unwrap();

        let p = match find_process(&mut sandbox, cid.as_str(), eid.as_str(), false) {
            Ok(v) => v,
            Err(_) => {
                let f = sink
                    .fail(RpcStatus::new(
                        RpcStatusCode::InvalidArgument,
                        Some(String::from("invalid argument")),
                    ))
                    .map_err(|_e| error!(sl!(), "invalid argument"));
                ctx.spawn(f);
                return;
            }
        };

        if p.term_master.is_some() {
            let _ = unistd::close(p.term_master.unwrap());
            p.term_master = None;
        }

        if p.parent_stdin.is_some() {
            let _ = unistd::close(p.parent_stdin.unwrap());
            p.parent_stdin = None;
        }

        let resp = Empty::new();

        let f = sink
            .success(resp)
            .map_err(|_e| error!(sl!(), "close stdin failed"));
        ctx.spawn(f);
    }

    fn tty_win_resize(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::TtyWinResizeRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        let cid = req.container_id.clone();
        let eid = req.exec_id.clone();
        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().unwrap();
        let p = find_process(&mut sandbox, cid.as_str(), eid.as_str(), false).unwrap();

        if p.term_master.is_none() {
            let f = sink
                .fail(RpcStatus::new(
                    RpcStatusCode::Unavailable,
                    Some("no tty".to_string()),
                ))
                .map_err(|_e| error!(sl!(), "tty resize"));
            ctx.spawn(f);
            return;
        }

        let fd = p.term_master.unwrap();
        unsafe {
            let win = winsize {
                ws_row: req.row as c_ushort,
                ws_col: req.column as c_ushort,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };

            let err = libc::ioctl(fd, TIOCSWINSZ, &win);
            if Errno::result(err).map(drop).is_err() {
                let f = sink
                    .fail(RpcStatus::new(
                        RpcStatusCode::Internal,
                        Some("ioctl error".to_string()),
                    ))
                    .map_err(|_e| error!(sl!(), "ioctl error!"));
                ctx.spawn(f);
                return;
            }
        }

        let empty = protocols::empty::Empty::new();
        let f = sink
            .success(empty)
            .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));
        ctx.spawn(f)
    }
    fn update_interface(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::UpdateInterfaceRequest,
        sink: ::grpcio::UnarySink<protocols::types::Interface>,
    ) {
        let interface = req.interface.clone();
        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().unwrap();

        if sandbox.rtnl.is_none() {
            sandbox.rtnl = Some(RtnlHandle::new(NETLINK_ROUTE, 0).unwrap());
        }

        let rtnl = sandbox.rtnl.as_mut().unwrap();

        let iface = match rtnl.update_interface(interface.as_ref().unwrap()) {
            Ok(v) => v,
            Err(_) => {
                let f = sink
                    .fail(RpcStatus::new(
                        RpcStatusCode::Internal,
                        Some("update interface".to_string()),
                    ))
                    .map_err(|_e| error!(sl!(), "update interface"));
                ctx.spawn(f);
                return;
            }
        };

        let f = sink
            .success(iface)
            .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));
        ctx.spawn(f)
    }
    fn update_routes(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::UpdateRoutesRequest,
        sink: ::grpcio::UnarySink<protocols::agent::Routes>,
    ) {
        let mut routes = protocols::agent::Routes::new();
        let rs = req.routes.clone().unwrap().Routes.into_vec();

        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().unwrap();

        if sandbox.rtnl.is_none() {
            sandbox.rtnl = Some(RtnlHandle::new(NETLINK_ROUTE, 0).unwrap());
        }

        let rtnl = sandbox.rtnl.as_mut().unwrap();
        // get current routes to return when error out
        let crs = match rtnl.list_routes() {
            Ok(routes) => routes,
            Err(_) => {
                let f = sink
                    .fail(RpcStatus::new(
                        RpcStatusCode::Internal,
                        Some("update routes".to_string()),
                    ))
                    .map_err(|_e| error!(sl!(), "update routes"));
                ctx.spawn(f);
                return;
            }
        };
        let v = match rtnl.update_routes(rs.as_ref()) {
            Ok(value) => value,
            Err(_) => crs,
        };

        routes.set_Routes(RepeatedField::from_vec(v));

        let f = sink
            .success(routes)
            .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));

        ctx.spawn(f)
    }
    fn list_interfaces(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::ListInterfacesRequest,
        sink: ::grpcio::UnarySink<protocols::agent::Interfaces>,
    ) {
        let mut interface = protocols::agent::Interfaces::new();
        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().unwrap();

        if sandbox.rtnl.is_none() {
            sandbox.rtnl = Some(RtnlHandle::new(NETLINK_ROUTE, 0).unwrap());
        }

        let rtnl = sandbox.rtnl.as_mut().unwrap();
        let v = match rtnl.list_interfaces() {
            Ok(value) => value,
            Err(_) => {
                let f = sink
                    .fail(RpcStatus::new(
                        RpcStatusCode::Internal,
                        Some("list interface".to_string()),
                    ))
                    .map_err(|_e| error!(sl!(), "list interface"));
                ctx.spawn(f);
                return;
            }
        };

        interface.set_Interfaces(RepeatedField::from_vec(v));

        let f = sink
            .success(interface)
            .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));
        ctx.spawn(f)
    }
    fn list_routes(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::ListRoutesRequest,
        sink: ::grpcio::UnarySink<protocols::agent::Routes>,
    ) {
        let mut routes = protocols::agent::Routes::new();
        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().unwrap();

        if sandbox.rtnl.is_none() {
            sandbox.rtnl = Some(RtnlHandle::new(NETLINK_ROUTE, 0).unwrap());
        }

        let rtnl = sandbox.rtnl.as_mut().unwrap();

        let v = match rtnl.list_routes() {
            Ok(value) => value,
            Err(_) => {
                let f = sink
                    .fail(RpcStatus::new(
                        RpcStatusCode::Internal,
                        Some("list routes".to_string()),
                    ))
                    .map_err(|_e| error!(sl!(), "list routes"));
                ctx.spawn(f);
                return;
            }
        };

        routes.set_Routes(RepeatedField::from_vec(v));

        let f = sink
            .success(routes)
            .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));
        ctx.spawn(f)
    }
    fn start_tracing(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::StartTracingRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        info!(sl!(), "start_tracing {:?} self.test={}", req, self.test);
        self.test = 2;
        let empty = protocols::empty::Empty::new();
        let f = sink
            .success(empty)
            .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));
        ctx.spawn(f)
    }
    fn stop_tracing(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::StopTracingRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        let empty = protocols::empty::Empty::new();
        let f = sink
            .success(empty)
            .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));
        ctx.spawn(f)
    }
    fn create_sandbox(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::CreateSandboxRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        let mut err = "".to_string();

        {
            let sandbox = self.sandbox.clone();
            let mut s = sandbox.lock().unwrap();

            let _ = fs::remove_dir_all(CONTAINER_BASE);
            let _ = fs::create_dir_all(CONTAINER_BASE);

            s.hostname = req.hostname.clone();
            s.running = true;

            if !req.sandbox_id.is_empty() {
                s.id = req.sandbox_id.clone();
            }

            match s.setup_shared_namespaces() {
                Ok(_) => (),
                Err(e) => err = e.to_string(),
            }
            if !err.is_empty() {
                let rpc_status =
                    grpcio::RpcStatus::new(grpcio::RpcStatusCode::FailedPrecondition, Some(err));
                let f = sink
                    .fail(rpc_status)
                    .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));
                ctx.spawn(f);
                return;
            }
        }

        match add_storages(sl!(), req.storages.to_vec(), &self.sandbox) {
            Ok(m) => {
                let sandbox = self.sandbox.clone();
                let mut s = sandbox.lock().unwrap();
                s.mounts = m
            }
            Err(e) => err = e.to_string(),
        };

        if !err.is_empty() {
            let rpc_status =
                grpcio::RpcStatus::new(grpcio::RpcStatusCode::FailedPrecondition, Some(err));
            let f = sink
                .fail(rpc_status)
                .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));
            ctx.spawn(f);
            return;
        }

        let empty = protocols::empty::Empty::new();
        let f = sink
            .success(empty)
            .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));
        ctx.spawn(f)
    }
    fn destroy_sandbox(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::DestroySandboxRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        let s = Arc::clone(&self.sandbox);
        let mut sandbox = s.lock().unwrap();
        // destroy all containers, clean up, notify agent to exit
        // etc.
        sandbox.destroy().unwrap();

        sandbox.sender.as_ref().unwrap().send(1).unwrap();
        sandbox.sender = None;

        let empty = protocols::empty::Empty::new();
        let f = sink
            .success(empty)
            .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));
        ctx.spawn(f)
    }
    fn online_cpu_mem(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::OnlineCPUMemRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        // sleep 5 seconds for debug
        // thread::sleep(Duration::new(5, 0));
        let s = Arc::clone(&self.sandbox);
        let sandbox = s.lock().unwrap();
        let empty = protocols::empty::Empty::new();

        if sandbox.online_cpu_memory(&req).is_err() {
            let f = sink
                .fail(RpcStatus::new(
                    RpcStatusCode::Internal,
                    Some("Internal error".to_string()),
                ))
                .map_err(|_e| error!(sl!(), "cannot online memory/cpu"));
            ctx.spawn(f);
            return;
        }

        let f = sink
            .success(empty)
            .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));

        ctx.spawn(f)
    }
    fn reseed_random_dev(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::ReseedRandomDevRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        let empty = protocols::empty::Empty::new();
        if random::reseed_rng(req.data.as_slice()).is_err() {
            let f = sink
                .fail(RpcStatus::new(
                    RpcStatusCode::Internal,
                    Some("Internal error".to_string()),
                ))
                .map_err(|_e| error!(sl!(), "fail to reseed rng!"));
            ctx.spawn(f);
            return;
        }

        let f = sink
            .success(empty)
            .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));
        ctx.spawn(f)
    }
    fn get_guest_details(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::GuestDetailsRequest,
        sink: ::grpcio::UnarySink<protocols::agent::GuestDetailsResponse>,
    ) {
        info!(sl!(), "get guest details!");
        let mut resp = GuestDetailsResponse::new();
        // to get memory block size
        match get_memory_info(req.mem_block_size, req.mem_hotplug_probe) {
            Ok((u, v)) => {
                resp.mem_block_size_bytes = u;
                resp.support_mem_hotplug_probe = v;
            }

            Err(_) => {
                info!(sl!(), "fail to get memory info!");
                let f = sink
                    .fail(RpcStatus::new(
                        RpcStatusCode::Internal,
                        Some(String::from("internal error")),
                    ))
                    .map_err(|_e| error!(sl!(), "cannot get memory info!"));
                ctx.spawn(f);
                return;
            }
        }

        // to get agent details
        let detail = get_agent_details();
        resp.agent_details = SingularPtrField::some(detail);

        let f = sink
            .success(resp)
            .map_err(|_e| error!(sl!(), "cannot get guest detail"));
        ctx.spawn(f);
    }
    fn mem_hotplug_by_probe(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::MemHotplugByProbeRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        let empty = protocols::empty::Empty::new();

        if do_mem_hotplug_by_probe(&req.memHotplugProbeAddr).is_err() {
            let f = sink
                .fail(RpcStatus::new(
                    RpcStatusCode::Internal,
                    Some("internal error!".to_string()),
                ))
                .map_err(|_e| error!(sl!(), "cannont mem hotplug by probe!"));
            ctx.spawn(f);
            return;
        }

        let f = sink
            .success(empty)
            .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));
        ctx.spawn(f)
    }
    fn set_guest_date_time(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::SetGuestDateTimeRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        let empty = protocols::empty::Empty::new();
        if do_set_guest_date_time(req.Sec, req.Usec).is_err() {
            let f = sink
                .fail(RpcStatus::new(
                    RpcStatusCode::Internal,
                    Some("internal error!".to_string()),
                ))
                .map_err(|_e| error!(sl!(), "cannot set guest time!"));
            ctx.spawn(f);
            return;
        }

        let f = sink
            .success(empty)
            .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));
        ctx.spawn(f)
    }
    fn copy_file(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::agent::CopyFileRequest,
        sink: ::grpcio::UnarySink<protocols::empty::Empty>,
    ) {
        let empty = protocols::empty::Empty::new();
        if do_copy_file(&req).is_err() {
            let f = sink
                .fail(RpcStatus::new(
                    RpcStatusCode::Internal,
                    Some("Internal error!".to_string()),
                ))
                .map_err(|_e| error!(sl!(), "cannot copy file!"));
            ctx.spawn(f);
            return;
        }

        let f = sink
            .success(empty)
            .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));
        ctx.spawn(f)
    }
}

#[derive(Clone)]
struct healthService;
impl protocols::health_grpc::Health for healthService {
    fn check(
        &mut self,
        ctx: ::grpcio::RpcContext,
        _req: protocols::health::CheckRequest,
        sink: ::grpcio::UnarySink<protocols::health::HealthCheckResponse>,
    ) {
        let mut resp = HealthCheckResponse::new();
        resp.set_status(HealthCheckResponse_ServingStatus::SERVING);

        let f = sink
            .success(resp)
            .map_err(|_e| error!(sl!(), "cannot get health status"));

        ctx.spawn(f);
    }
    fn version(
        &mut self,
        ctx: ::grpcio::RpcContext,
        req: protocols::health::CheckRequest,
        sink: ::grpcio::UnarySink<protocols::health::VersionCheckResponse>,
    ) {
        info!(sl!(), "version {:?}", req);
        let mut rep = protocols::health::VersionCheckResponse::new();
        rep.agent_version = AGENT_VERSION.to_string();
        rep.grpc_version = API_VERSION.to_string();
        let f = sink
            .success(rep)
            .map_err(move |e| error!(sl!(), "failed to reply {:?}: {:?}", req, e));
        ctx.spawn(f)
    }
}

fn get_memory_info(block_size: bool, hotplug: bool) -> Result<(u64, bool)> {
    let mut size: u64 = 0;
    let mut plug: bool = false;
    if block_size {
        match fs::read_to_string(SYSFS_MEMORY_BLOCK_SIZE_PATH) {
            Ok(v) => {
                if v.is_empty() {
                    info!(sl!(), "string in empty???");
                    return Err(ErrorKind::ErrorCode("Invalid block size".to_string()).into());
                }

                size = v.trim().parse::<u64>()?;
            }
            Err(e) => {
                info!(sl!(), "memory block size error: {:?}", e.kind());
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(ErrorKind::Io(e).into());
                }
            }
        }
    }

    if hotplug {
        match stat::stat(SYSFS_MEMORY_HOTPLUG_PROBE_PATH) {
            Ok(_) => plug = true,
            Err(e) => {
                info!(
                    sl!(),
                    "hotplug memory error: {}",
                    e.as_errno().unwrap().desc()
                );
                match e {
                    nix::Error::Sys(errno) => match errno {
                        Errno::ENOENT => plug = false,
                        _ => return Err(ErrorKind::Nix(e).into()),
                    },
                    _ => return Err(ErrorKind::Nix(e).into()),
                }
            }
        }
    }

    Ok((size, plug))
}

fn get_agent_details() -> AgentDetails {
    let mut detail = AgentDetails::new();

    detail.set_version(AGENT_VERSION.to_string());
    detail.set_supports_seccomp(false);
    detail.init_daemon = { unistd::getpid() == Pid::from_raw(1) };

    detail.device_handlers = RepeatedField::new();
    detail.storage_handlers = RepeatedField::from_vec(
        STORAGEHANDLERLIST
            .keys()
            .cloned()
            .map(|x| x.into())
            .collect(),
    );

    detail
}

fn read_stream(fd: RawFd, l: usize) -> Result<Vec<u8>> {
    let mut v: Vec<u8> = Vec::with_capacity(l);
    unsafe {
        v.set_len(l);
    }

    match unistd::read(fd, v.as_mut_slice()) {
        Ok(len) => {
            v.resize(len, 0);
            // Rust didn't return an EOF error when the reading peer point
            // was closed, instead it would return a 0 reading length, please
            // see https://github.com/rust-lang/rfcs/blob/master/text/0517-io-os-reform.md#errors
            if len == 0 {
                return Err(ErrorKind::ErrorCode("read  meet eof".to_string()).into());
            }
        }
        Err(e) => match e {
            nix::Error::Sys(errno) => match errno {
                Errno::EAGAIN => v.resize(0, 0),
                _ => return Err(ErrorKind::Nix(nix::Error::Sys(errno)).into()),
            },
            _ => return Err(ErrorKind::ErrorCode("read error".to_string()).into()),
        },
    }

    Ok(v)
}

fn find_process<'a>(
    sandbox: &'a mut Sandbox,
    cid: &'a str,
    eid: &'a str,
    init: bool,
) -> Result<&'a mut Process> {
    let ctr = match sandbox.get_container(cid) {
        Some(v) => v,
        None => return Err(ErrorKind::ErrorCode(String::from("Invalid container id")).into()),
    };

    if init && eid == "" {
        let p = match ctr.processes.get_mut(&ctr.init_process_pid) {
            Some(v) => v,
            None => {
                return Err(ErrorKind::ErrorCode(String::from("cannot find init process!")).into())
            }
        };

        return Ok(p);
    }

    let p = match ctr.get_process(eid) {
        Ok(v) => v,
        Err(_) => return Err(ErrorKind::ErrorCode("Invalid exec id".to_string()).into()),
    };

    Ok(p)
}

pub fn start<S: Into<String>>(sandbox: Arc<Mutex<Sandbox>>, host: S, port: u16) -> Server {
    let env = Arc::new(
        EnvBuilder::new()
            .cq_count(1)
            .wait_thread_count_default(5)
            .wait_thread_count_min(1)
            .wait_thread_count_max(10)
            .build(),
    );
    let worker = agentService { sandbox, test: 1 };
    let service = protocols::agent_grpc::create_agent_service(worker);
    let hservice = protocols::health_grpc::create_health(healthService);
    let mut server = ServerBuilder::new(env)
        .register_service(service)
        .register_service(hservice)
        .requests_slot_per_cq(1024)
        .bind(host, port)
        .build()
        .unwrap();
    server.start();
    info!(sl!(), "gRPC server started");
    for &(ref host, port) in server.bind_addrs() {
        info!(sl!(), "listening"; "host" => host, "port" => port);
    }

    server
}

// This function updates the container namespaces configuration based on the
// sandbox information. When the sandbox is created, it can be setup in a way
// that all containers will share some specific namespaces. This is the agent
// responsibility to create those namespaces so that they can be shared across
// several containers.
// If the sandbox has not been setup to share namespaces, then we assume all
// containers will be started in their own new namespace.
// The value of a.sandbox.sharedPidNs.path will always override the namespace
// path set by the spec, since we will always ignore it. Indeed, it makes no
// sense to rely on the namespace path provided by the host since namespaces
// are different inside the guest.
fn update_container_namespaces(sandbox: &Sandbox, spec: &mut Spec) -> Result<()> {
    let linux = match spec.Linux.as_mut() {
        None => {
            return Err(
                ErrorKind::ErrorCode("Spec didn't container linux field".to_string()).into(),
            )
        }
        Some(l) => l,
    };

    let mut pidNs = false;

    let namespaces = linux.Namespaces.as_mut_slice();
    for namespace in namespaces.iter_mut() {
        match namespace.Type.as_str() {
            NSTYPEPID => pidNs = true,
            NSTYPEIPC => namespace.Path = sandbox.shared_ipcns.path.clone(),
            NSTYPEUTS => namespace.Path = sandbox.shared_utsns.path.clone(),
            _ => {}
        }
    }

    if !pidNs && !sandbox.sandbox_pid_ns {
        let mut pid_ns = LinuxNamespace::new();
        pid_ns.set_Type(NSTYPEPID.to_string());
        linux.Namespaces.push(pid_ns);
    }

    Ok(())
}

// Check is the container process installed the
// handler for specific signal.
fn is_signal_handled(pid: pid_t, signum: u32) -> bool {
    let sig_mask: u64 = 1u64 << (signum - 1);
    let file_name = format!("/proc/{}/status", pid);

    // Open the file in read-only mode (ignoring errors).
    let file = match File::open(&file_name) {
        Ok(f) => f,
        Err(_) => {
            warn!(sl!(), "failed to open file {}\n", file_name);
            return false;
        }
    };

    let reader = BufReader::new(file);

    // Read the file line by line using the lines() iterator from std::io::BufRead.
    for (_index, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(_) => {
                warn!(sl!(), "failed to read file {}\n", file_name);
                return false;
            }
        };
        if line.starts_with("SigCgt:") {
            let mask_vec: Vec<&str> = line.split(':').collect();
            if mask_vec.len() != 2 {
                warn!(sl!(), "parse the SigCgt field failed\n");
                return false;
            }
            let sig_cgt_str = mask_vec[1];
            let sig_cgt_mask = match u64::from_str_radix(sig_cgt_str, 16) {
                Ok(h) => h,
                Err(_) => {
                    warn!(sl!(), "failed to parse the str {} to hex\n", sig_cgt_str);
                    return false;
                }
            };

            return (sig_cgt_mask & sig_mask) == sig_mask;
        }
    }
    false
}

fn do_mem_hotplug_by_probe(addrs: &[u64]) -> Result<()> {
    for addr in addrs.iter() {
        fs::write(SYSFS_MEMORY_HOTPLUG_PROBE_PATH, format!("{:#X}", *addr))?;
    }
    Ok(())
}

fn do_set_guest_date_time(sec: i64, usec: i64) -> Result<()> {
    let tv = libc::timeval {
        tv_sec: sec,
        tv_usec: usec,
    };

    let ret = unsafe { libc::settimeofday(&tv as *const libc::timeval, std::ptr::null()) };

    Errno::result(ret).map(drop)?;

    Ok(())
}

fn do_copy_file(req: &CopyFileRequest) -> Result<()> {
    let path = PathBuf::from(&req.path);

    if !path.starts_with(CONTAINER_BASE) {
        return error_code(Errno::EINVAL);
    }

    let dir = match path.parent() {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from("/"),
    };
    let dir_str = match dir.to_str() {
        Some(v) => v,
        None => return error_code(Errno::EINVAL),
    };

    if let Err(e) = fs::create_dir_all(dir_str) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(e.into());
        }
    }

    std::fs::set_permissions(dir_str, std::fs::Permissions::from_mode(req.dir_mode))?;

    let mut tmpfile = path.clone();
    tmpfile.set_extension("tmp");
    // Safe to unwrap because the composed tmp file name should be valid.
    let tmpfile_str = tmpfile.to_str().unwrap();

    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(tmpfile_str)?;

    file.write_all_at(req.data.as_slice(), req.offset as u64)?;
    let st = stat::stat(tmpfile_str)?;

    if st.st_size != req.file_size {
        return Ok(());
    }

    file.set_permissions(std::fs::Permissions::from_mode(req.file_mode))?;

    unistd::chown(
        tmpfile_str,
        Some(Uid::from_raw(req.uid as u32)),
        Some(Gid::from_raw(req.gid as u32)),
    )?;

    fs::rename(tmpfile, path)?;

    Ok(())
}

fn setup_bundle(gspec: &Spec) -> Result<()> {
    let root = match gspec.Root.as_ref() {
        Some(r) => r.Path.as_str(),
        None => return error_code(Errno::EINVAL),
    };
    let rootfs = fs::canonicalize(root)?;
    let bundle_path = match rootfs.parent() {
        Some(p) => match p.to_str() {
            Some(v) => v,
            None => return error_code(Errno::EINVAL),
        },
        None => return error_code(Errno::EINVAL),
    };

    let oci = rustjail::grpc_to_oci(gspec);
    match oci.process.as_ref() {
        Some(v) => info!(sl!(), "{:?}", v.console_size.as_ref()),
        None => debug!(sl!(), "no process info available."),
    }

    let config = format!("{}/{}", bundle_path, "config.json");
    let _ = oci.save(config.as_str());

    unistd::chdir(bundle_path)?;

    Ok(())
}

#[inline]
fn error_code<T>(errno: Errno) -> Result<T> {
    Err(ErrorKind::Nix(nix::Error::from_errno(errno)).into())
}
