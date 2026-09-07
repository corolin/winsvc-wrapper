//! Service lifecycle commands: install / uninstall / start / stop / restart /
//! status / refresh.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, bail};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::account::ensure_logon_as_service;
use crate::config::{FailureAction, Resolved, StartType};
use crate::sddl::apply_security_descriptor;

fn winapi_code(e: &windows_service::Error) -> Option<i32> {
    match e {
        windows_service::Error::Winapi(io) => io.raw_os_error(),
        _ => None,
    }
}

fn admin_hint(e: windows_service::Error) -> anyhow::Error {
    if winapi_code(&e) == Some(5) {
        anyhow::anyhow!("{e}\nhint: managing services requires an elevated (administrator) prompt")
    } else {
        anyhow::anyhow!(e)
    }
}

fn connect_manager() -> anyhow::Result<ServiceManager> {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::ALL_ACCESS)
        .map_err(admin_hint)
}

fn open_service(
    manager: &ServiceManager,
    id: &str,
    access: ServiceAccess,
) -> anyhow::Result<windows_service::service::Service> {
    manager.open_service(id, access).map_err(|e| {
        if winapi_code(&e) == Some(1060) {
            anyhow::anyhow!("service `{id}` is not installed (run `rsw install` first)")
        } else {
            admin_hint(e)
        }
    })
}

fn current_exe() -> anyhow::Result<PathBuf> {
    dunce::canonicalize(std::env::current_exe().context("locating the rsw executable")?)
        .context("canonicalizing the rsw executable path")
}

fn start_type_of(cfg: &StartType) -> ServiceStartType {
    match cfg {
        StartType::Auto | StartType::Delayed => ServiceStartType::AutoStart,
        StartType::Manual => ServiceStartType::OnDemand,
        StartType::Disabled => ServiceStartType::Disabled,
        StartType::Boot => ServiceStartType::BootStart,
        StartType::System => ServiceStartType::SystemStart,
    }
}

fn failure_actions(resolved: &Resolved) -> ServiceFailureActions {
    let actions: Vec<ServiceAction> = resolved
        .cfg
        .on_failure
        .iter()
        .map(|a| ServiceAction {
            action_type: match a.action {
                FailureAction::Restart => ServiceActionType::Restart,
                FailureAction::Reboot => ServiceActionType::Reboot,
                FailureAction::None => ServiceActionType::None,
            },
            delay: Duration::from_millis(a.delay),
        })
        .collect();
    ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(
            resolved.cfg.service.failure_reset_after / 1000,
        )),
        reboot_msg: None,
        command: None,
        // Some([]) (cActions=0 with a non-NULL array) DELETES all recovery
        // actions; None means "leave unchanged", so refresh could never clear.
        actions: Some(actions),
    }
}

/// Applies every post-create service property (shared by install & refresh).
fn apply_service_properties(
    service: &windows_service::service::Service,
    resolved: &Resolved,
) -> anyhow::Result<()> {
    let svc = &resolved.cfg.service;
    service
        .set_description(&svc.description)
        .map_err(admin_hint)?;
    if svc.start_type == StartType::Delayed {
        service.set_delayed_auto_start(true).map_err(admin_hint)?;
    }
    // Always write failure actions: `Some([])` clears any actions left over
    // from an earlier config, so removing [[on_failure]] and refreshing
    // really removes the recovery actions from the SCM.
    service
        .update_failure_actions(failure_actions(resolved))
        .map_err(admin_hint)?;
    service
        .set_failure_actions_on_non_crash_failures(true)
        .map_err(admin_hint)?;
    if svc.preshutdown {
        service
            .set_preshutdown_timeout(Duration::from_secs(svc.preshutdown_timeout_secs))
            .map_err(admin_hint)?;
    }
    if let Some(sddl) = &svc.security_descriptor {
        apply_security_descriptor(resolved.service_id(), sddl)?;
    }
    Ok(())
}

pub fn install(resolved: &Resolved) -> anyhow::Result<u32> {
    let svc = &resolved.cfg.service;
    let exe = current_exe()?;
    let config_path = dunce::canonicalize(&resolved.config_path)
        .with_context(|| format!("canonicalizing {}", resolved.config_path.display()))?;

    let manager = connect_manager()?;
    let info = ServiceInfo {
        name: svc.id.clone().into(),
        display_name: resolved.display_name().into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: start_type_of(&svc.start_type),
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec![
            "run-service".into(),
            "--config".into(),
            config_path.clone().into_os_string(),
        ],
        dependencies: svc
            .dependencies
            .iter()
            .map(|d| windows_service::service::ServiceDependency::Service(d.as_str().into()))
            .collect(),
        account_name: svc.account.as_ref().map(|a| a.username.as_str().into()),
        account_password: svc
            .account
            .as_ref()
            .filter(|a| !a.password.is_empty())
            .map(|a| a.password.as_str().into()),
    };
    let service = manager
        .create_service(&info, ServiceAccess::ALL_ACCESS)
        .map_err(|e| {
            if winapi_code(&e) == Some(1073) {
                anyhow::anyhow!(
                    "service `{}` already exists; uninstall first or change [service] id",
                    svc.id
                )
            } else {
                admin_hint(e)
            }
        })?;
    println!("service `{}` installed", svc.id);

    if let Some(account) = &svc.account {
        match ensure_logon_as_service(account) {
            Ok(note) => println!("account: {note}"),
            Err(e) => eprintln!(
                "warning: {e:#}\nthe service is installed, but may fail to start until the right is granted"
            ),
        }
    }

    apply_service_properties(&service, resolved)?;
    println!(
        "properties applied (start type: {}, failure actions: {})",
        format!("{:?}", svc.start_type).to_lowercase(),
        resolved.cfg.on_failure.len()
    );
    Ok(0)
}

pub fn uninstall(resolved: &Resolved) -> anyhow::Result<u32> {
    let manager = connect_manager()?;
    let service = open_service(&manager, resolved.service_id(), ServiceAccess::ALL_ACCESS)?;
    if let Ok(status) = service.query_status()
        && status.current_state != ServiceState::Stopped
    {
        println!("service is {:?}; stopping first...", status.current_state);
        let _ = service.stop();
        wait_for_state(&service, ServiceState::Stopped, Duration::from_secs(60))?;
    }
    service.delete().map_err(admin_hint)?;
    println!("service `{}` uninstalled", resolved.service_id());
    Ok(0)
}

pub fn start(resolved: &Resolved) -> anyhow::Result<u32> {
    let manager = connect_manager()?;
    let service = open_service(&manager, resolved.service_id(), ServiceAccess::START)?;
    service.start(&[] as &[&str]).map_err(admin_hint)?;
    println!("service `{}` started", resolved.service_id());
    Ok(0)
}

pub fn stop(resolved: &Resolved) -> anyhow::Result<u32> {
    let manager = connect_manager()?;
    let service = open_service(&manager, resolved.service_id(), ServiceAccess::STOP)?;
    service.stop().map_err(admin_hint)?;
    println!("service `{}` stop requested", resolved.service_id());
    Ok(0)
}

pub fn restart(resolved: &Resolved) -> anyhow::Result<u32> {
    let manager = connect_manager()?;
    let service = open_service(
        &manager,
        resolved.service_id(),
        ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
    )?;
    if let Ok(status) = service.query_status()
        && status.current_state != ServiceState::Stopped
    {
        let _ = service.stop();
        wait_for_state(&service, ServiceState::Stopped, Duration::from_secs(60))?;
    }
    service.start(&[] as &[&str]).map_err(admin_hint)?;
    println!("service `{}` restarted", resolved.service_id());
    Ok(0)
}

/// WinSW-compatible status semantics:
/// exit 0 = running/stopped/paused per rsw (WinSW treats Paused as 1),
/// 1 = transitional, 1060 = not installed.
pub fn status(resolved: &Resolved) -> anyhow::Result<u32> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(admin_hint)?;
    let service = match manager.open_service(resolved.service_id(), ServiceAccess::QUERY_STATUS) {
        Ok(s) => s,
        Err(e) if winapi_code(&e) == Some(1060) => {
            println!("NonExistent");
            return Ok(1060);
        }
        Err(e) => return Err(admin_hint(e)),
    };
    let status = service.query_status().map_err(admin_hint)?;
    let state = status.current_state;
    println!("service `{}`: {:?}", resolved.service_id(), state);
    match state {
        ServiceState::Running | ServiceState::Stopped | ServiceState::Paused => Ok(0),
        ServiceState::StartPending
        | ServiceState::StopPending
        | ServiceState::ContinuePending
        | ServiceState::PausePending => Ok(1),
    }
}

pub fn refresh(resolved: &Resolved) -> anyhow::Result<u32> {
    let manager = connect_manager()?;
    let service = open_service(&manager, resolved.service_id(), ServiceAccess::ALL_ACCESS)?;
    let svc = &resolved.cfg.service;
    let config_path = dunce::canonicalize(&resolved.config_path)?;
    let info = ServiceInfo {
        name: svc.id.clone().into(),
        display_name: resolved.display_name().into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: start_type_of(&svc.start_type),
        error_control: ServiceErrorControl::Normal,
        executable_path: current_exe()?,
        launch_arguments: vec![
            "run-service".into(),
            "--config".into(),
            config_path.into_os_string(),
        ],
        dependencies: svc
            .dependencies
            .iter()
            .map(|d| windows_service::service::ServiceDependency::Service(d.as_str().into()))
            .collect(),
        account_name: svc.account.as_ref().map(|a| a.username.as_str().into()),
        account_password: svc
            .account
            .as_ref()
            .filter(|a| !a.password.is_empty())
            .map(|a| a.password.as_str().into()),
    };
    service.change_config(&info).map_err(admin_hint)?;
    if svc.start_type != StartType::Delayed {
        service.set_delayed_auto_start(false).map_err(admin_hint)?;
    }
    apply_service_properties(&service, resolved)?;
    println!(
        "service `{}` refreshed from {}",
        resolved.service_id(),
        resolved.config_path.display()
    );
    Ok(0)
}

fn wait_for_state(
    service: &windows_service::service::Service,
    want: ServiceState,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let state = service
            .query_status()
            .map(|s| s.current_state)
            .unwrap_or(ServiceState::Stopped);
        if state == want {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!("timed out waiting for service to become {want:?} (now {state:?})");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
