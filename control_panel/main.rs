use std::panic::AssertUnwindSafe;

use control_panel::ControlPanel;
use futures::FutureExt;
use mmb_utils::{
    infrastructure::init_infrastructure,
    logger::print_info,
    panic::{PanicState, HOOK_IS_NOT_SET, PANIC_DETECTED_IN_NO_PANIC_STATE, PANIC_STATE},
};
use tokio::signal;

mod control_panel;
mod endpoints;

static ADDRESS: &str = "127.0.0.1:8080";

async fn control_panel_run() {
    let control_panel = ControlPanel::new(ADDRESS).await;

    control_panel
        .clone()
        .start()
        .expect("Unable to start control panel")
        .join()
        .expect("control panel finished with error");

    signal::ctrl_c().await.expect("failed to listen for event");

    log::info!("Ctrl-C signal was received so control_panel will be stopped");

    control_panel
        .stop()
        .expect("failed to get stop receiver")
        .await
        .expect("Failed to get work finished message")
        .expect("Failed to stop control panel");

    print_info("ControlPanel has been stopped");
}

#[actix_web::main]
async fn main() {
    init_infrastructure("control_panel_log.txt");

    if let Err(_) = AssertUnwindSafe(control_panel_run()).catch_unwind().await {
        PANIC_STATE.with(|panic_state| {
            match &*panic_state.borrow() {
                PanicState::PanicHookIsNotSet => log::warn!("{HOOK_IS_NOT_SET}"),
                PanicState::NoPanic => log::error!("{PANIC_DETECTED_IN_NO_PANIC_STATE}"),
                PanicState::PanicHappened(msg) => log::error!("{msg}"),
            };
        });
    }
}
