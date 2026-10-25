//! Spate desktop app.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod clipwatch;
mod dialogs;
mod format;
mod icon_art;
mod icons;
mod segmap;
mod sys;
mod theme;
mod tray;
mod update;
mod urls;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, OnceLock, mpsc};
use std::time::Duration;

use eframe::egui;
use spate_core::ipc::{self, AddItem, Request, Response};
use spate_core::{Manager, paths};

use crate::app::{App, UiEvent};

fn main() -> ExitCode {
    let args = urls::parse_args(std::env::args().skip(1));
    let data_dir = paths::data_dir();

    // Single instance: hand everything to the running app and leave.
    if ipc::is_running(&data_dir) {
        let request = if args.urls.is_empty() {
            Request::Show
        } else {
            Request::Add {
                items: args.urls.iter().map(|u| AddItem { url: u.clone(), ..Default::default() }).collect(),
                interactive: true,
                start: None,
            }
        };
        return match ipc::send(&data_dir, &request) {
            Ok(r) if r.ok => ExitCode::SUCCESS,
            Ok(r) => fail(&format!("Spate refused the request: {}", r.error.unwrap_or_default())),
            Err(e) => fail(&format!("Could not reach the running Spate: {e}")),
        };
    }

    // Files an earlier update replaced; the version that ran from them may
    // take a moment to exit after starting this one.
    let _ = std::thread::Builder::new().name("spate-cleanup".into()).spawn(|| {
        std::thread::sleep(Duration::from_secs(10));
        update::clean_up();
    });

    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("spate-rt").build() {
        Ok(rt) => rt,
        Err(e) => return fail(&format!("Could not start the async runtime: {e}")),
    };
    let open = if args.updated { Manager::new_resuming } else { Manager::new };
    let manager = match open(rt.handle().clone(), data_dir.clone()) {
        Ok(m) => m,
        Err(e) => return fail(&format!("Could not load the download list from {}:\n{e}", data_dir.display())),
    };

    let (tx, rx) = mpsc::channel::<UiEvent>();
    let ctx_cell: Arc<OnceLock<egui::Context>> = Arc::new(OnceLock::new());

    let handler: ipc::Handler = {
        let tx = tx.clone();
        let ctx_cell = ctx_cell.clone();
        Arc::new(move |req: Request| {
            if matches!(req, Request::Ping) {
                return Response::ok();
            }
            if tx.send(UiEvent::Ipc(req)).is_err() {
                return Response::err("Spate is shutting down");
            }
            if let Some(ctx) = ctx_cell.get() {
                ctx.request_repaint();
            }
            Response::ok()
        })
    };
    let server = match rt.block_on(ipc::serve(&data_dir, handler)) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("IPC server unavailable: {e}");
            None
        }
    };

    {
        let (tx, ctx_cell) = (tx.clone(), ctx_cell.clone());
        manager.set_on_complete(move |id, path| {
            let _ = tx.send(UiEvent::Completed(id, path));
            if let Some(ctx) = ctx_cell.get() {
                ctx.request_repaint();
            }
        });
    }
    {
        let ctx_cell = ctx_cell.clone();
        manager.set_on_change(move || {
            if let Some(ctx) = ctx_cell.get() {
                ctx.request_repaint();
            }
        });
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Spate")
            .with_app_id("Spate")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([720.0, 420.0])
            .with_icon(Arc::new(icons::app_icon(64))),
        persistence_path: Some(data_dir.join("window.ron")),
        centered: true,
        ..Default::default()
    };

    let m = manager.clone();
    let restart: Arc<OnceLock<PathBuf>> = Arc::new(OnceLock::new());
    let app_restart = restart.clone();
    let result = eframe::run_native(
        "Spate",
        options,
        Box::new(move |cc| {
            let _ = ctx_cell.set(cc.egui_ctx.clone());
            Ok(Box::new(App::new(cc, m, rx, tx, args, app_restart)))
        }),
    );

    // Stop accepting requests before winding down, so a new launch starts
    // a fresh instance instead of talking to this one.
    drop(server);
    manager.shutdown();
    rt.shutdown_timeout(Duration::from_secs(3));

    // An installed update starts once the list is saved and this instance
    // no longer answers, so it does not hand itself over to us.
    if let Some(exe) = restart.get()
        && let Err(e) = std::process::Command::new(exe).arg(urls::UPDATED_FLAG).spawn()
    {
        return fail(&format!("Spate was updated, but the new version could not be started: {e}\nStart {} again.", exe.display()));
    }

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&format!("The window could not be created: {e}")),
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("{msg}");
    let _ = rfd::MessageDialog::new().set_title("Spate").set_description(msg).set_level(rfd::MessageLevel::Error).show();
    ExitCode::FAILURE
}
