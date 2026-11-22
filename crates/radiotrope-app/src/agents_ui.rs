//! The Agents dialog, and the network server it turns on and off

use std::sync::Arc;

use slint::ComponentHandle;

use crate::{mcp, AgentRow, App};

/// Wire the Agents (MCP) dialog, and start network agents when they are on.
/// `agents` is `None` in a `--mcp --standalone` player, which serves one
/// agent on stdio only: the dialog shows the settings greyed out. Keep the
/// returned timer: it starts the network server again when the network
/// changes.
pub(crate) fn setup(
    ui: &App,
    agents: Option<Arc<mcp::agents::Agents>>,
    settings: &radiotrope_app::data::settings::Settings,
) -> slint::Timer {
    use mcp::setup::AgentApp;
    use radiotrope_app::data::settings::McpAuth;
    ui.set_agents_available(agents.is_some());
    ui.set_agents_network(settings.mcp_network);
    let listen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    show_listen_choices(ui, settings, &listen);
    let auths: Vec<slint::SharedString> = McpAuth::ALL.iter().map(|a| a.label().into()).collect();
    ui.set_agents_auths(std::rc::Rc::new(slint::VecModel::from(auths)).into());
    let auth = McpAuth::ALL
        .iter()
        .position(|a| *a == settings.mcp_auth)
        .unwrap_or(0);
    ui.set_agents_auth(auth as i32);

    // Setup lines for the agent picked in the dialog
    let labels: Vec<slint::SharedString> =
        AgentApp::ALL.iter().map(|app| app.label().into()).collect();
    ui.set_agents_apps(std::rc::Rc::new(slint::VecModel::from(labels)).into());
    let picked = AgentApp::from_id(settings.mcp_client.as_deref());
    let index = AgentApp::ALL
        .iter()
        .position(|app| *app == picked)
        .unwrap_or(0);
    ui.set_agents_app(index as i32);
    ui.on_agents_app_changed({
        let ui_weak = ui.as_weak();
        move |index| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let Some(app) = AgentApp::ALL.get(index as usize) else {
                return;
            };
            save_agent_settings(|s| s.mcp_client = Some(app.id().to_string()));
            show_agent_lines(&ui);
        }
    });
    show_agent_lines(ui);

    let timer = slint::Timer::default();
    let Some(agents) = agents else { return timer };
    agents.apply_network_later(settings, show_agents_network_later(ui));

    // Every change saves and restarts the network server with it
    let apply = {
        let ui_weak = ui.as_weak();
        let agents = agents.clone();
        let listen_for_apply = listen.clone();
        move |change: &dyn Fn(&mut radiotrope_app::data::settings::Settings)| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let settings = crate::window_settings::update_settings(change);
            show_listen_choices(&ui, &settings, &listen_for_apply);
            // A change of "Listen on" may have turned the token on
            if let Some(auth) = McpAuth::ALL.iter().position(|a| *a == settings.mcp_auth) {
                ui.set_agents_auth(auth as i32);
            }
            agents.apply_network_later(&settings, show_agents_network_later(&ui));
        }
    };
    let apply = std::rc::Rc::new(apply);

    ui.on_agents_network_toggled({
        let apply = apply.clone();
        move |on| apply(&|s| s.mcp_network = on)
    });
    // Authentication picked by hand in this session: listening on a
    // network then leaves it as it is
    let auth_picked = std::rc::Rc::new(std::cell::Cell::new(false));
    ui.on_agents_auth_changed({
        let apply = apply.clone();
        let auth_picked = auth_picked.clone();
        move |index| {
            let Some(auth) = McpAuth::ALL.get(index as usize).copied() else {
                return;
            };
            auth_picked.set(true);
            apply(&|s| s.mcp_auth = auth)
        }
    });
    ui.on_agents_listen_changed({
        let apply = apply.clone();
        let listen = listen.clone();
        move |index| {
            let Some(choice) = listen.borrow().get(index as usize).cloned() else {
                return;
            };
            apply(&|s| {
                let was_local = listens_locally(s);
                s.mcp_address = std::net::SocketAddr::new(choice.ip, saved_port(s)).to_string();
                s.mcp_interface = choice.interface.clone();
                s.mcp_auth = safer_auth(s.mcp_auth, was_local, choice.ip, auth_picked.get());
            })
        }
    });
    ui.on_agents_apply_port({
        let apply = apply.clone();
        let ui_weak = ui.as_weak();
        move |text| {
            let text = text.trim();
            let port = if text.is_empty() {
                Some(default_port())
            } else {
                text.parse::<u16>().ok().filter(|p| *p != 0)
            };
            let Some(port) = port else {
                // Not a port: show the saved one again
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_agents_edit_port(ui.get_agents_port());
                }
                return;
            };
            apply(&|s| {
                let ip = mcp::network::split_address(&s.mcp_address)
                    .map(|a| a.ip())
                    .unwrap_or(std::net::Ipv4Addr::LOCALHOST.into());
                s.mcp_address = std::net::SocketAddr::new(ip, port).to_string();
            })
        }
    });
    ui.on_agents_refresh_listen({
        let ui_weak = ui.as_weak();
        let listen = listen.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let settings = radiotrope_app::data::settings::Settings::load().unwrap_or_default();
            show_listen_choices(&ui, &settings, &listen);
        }
    });
    ui.on_agents_regenerate_token({
        let apply = apply.clone();
        let ui_weak = ui.as_weak();
        move || {
            let error = match radiotrope_app::data::agent_token::regenerate() {
                Ok(_) => String::new(),
                Err(e) => {
                    eprintln!("Failed to make a new token: {e}");
                    token_error(&e)
                }
            };
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_agents_token_error(error.into());
            }
            apply(&|_| {})
        }
    });

    // A picked interface gets a new address with a new lease, and at login
    // the network may come up after the player: listen again then
    let ui_weak = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        radiotrope_app::config::mcp::NETWORK_RECHECK,
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if let Some(settings) = agents.network_to_recheck() {
                agents.apply_network_later(&settings, show_agents_network_later(&ui));
            }
        },
    );
    timer
}

/// Keep the menu bar's agents chip up to date: who uses the player now
pub(crate) fn watch(ui: &App, presence: Option<mcp::presence::Presence>) -> slint::Timer {
    let timer = slint::Timer::default();
    let Some(presence) = presence else {
        return timer;
    };
    let ui_weak = ui.as_weak();
    // Compared as shown, so "4 min ago" redraws once a minute, not every tick
    let mut shown: Option<Vec<AgentRow>> = None;
    timer.start(
        slint::TimerMode::Repeated,
        radiotrope_app::config::ui::AGENTS_REFRESH,
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let rows: Vec<AgentRow> = presence
                .agents()
                .iter()
                .map(|a| AgentRow {
                    name: a.name().into(),
                    place: a.place_label().into(),
                    network: a.is_network(),
                    seen: a.seen_label().into(),
                    connected: a.is_connected(),
                })
                .collect();
            if shown.as_ref() == Some(&rows) {
                return;
            }
            ui.set_agents_connected(connected_line(&rows).into());
            ui.set_agent_count(rows.len() as i32);
            ui.set_agent_list(std::rc::Rc::new(slint::VecModel::from(rows.clone())).into());
            shown = Some(rows);
        },
    );
    timer
}

/// Show the network status once the server is set up, on its own thread
fn show_agents_network_later(ui: &App) -> mcp::agents::ShowStatus {
    let ui_weak = ui.as_weak();
    Box::new(move |status| {
        let _ = ui_weak.upgrade_in_event_loop(move |ui| show_agents_network(&ui, &status));
    })
}

fn show_agents_network(ui: &App, status: &mcp::agents::NetworkStatus) {
    ui.set_agents_status(status.text.as_str().into());
    ui.set_agents_status_error(status.is_error);
    ui.set_agents_token(status.token.as_str().into());
    ui.set_agents_url(status.url.as_str().into());
    show_agent_lines(ui);
}

/// The lines that add Radiotrope to the agent picked in the dialog
pub(crate) fn show_agent_lines(ui: &App) {
    use mcp::setup::{self, AgentApp};
    use radiotrope_app::data::settings::McpAuth;
    let app = AgentApp::ALL
        .get(ui.get_agents_app().max(0) as usize)
        .copied()
        .unwrap_or(AgentApp::ClaudeCode);
    let url = ui.get_agents_url();
    let token = ui.get_agents_token();
    let wants_token =
        McpAuth::ALL.get(ui.get_agents_auth().max(0) as usize) == Some(&McpAuth::Token);
    ui.set_agents_local_command(setup::local_line(app, &setup::this_program()).into());
    let network = (!url.is_empty())
        .then(|| setup::network_line(app, &url, wants_token.then_some(token.as_str())))
        .flatten();
    ui.set_agents_command(network.unwrap_or_default().into());
    ui.set_agents_network_placeholder(setup::no_network_line(app).into());
    ui.set_agents_local_note(app.local_note().into());
    ui.set_agents_network_note(app.network_note(wants_token).into());
}

/// One entry of the Agents dialog's "Listen on" list
#[derive(Clone)]
struct ListenChoice {
    pub(crate) ip: std::net::IpAddr,
    /// The interface it belongs to, saved so its next address is used too
    pub(crate) interface: Option<String>,
}

/// Fill the "Listen on" list: this computer only, each network interface,
/// then all networks; and pick the saved one (added if it's not there)
fn show_listen_choices(
    ui: &App,
    settings: &radiotrope_app::data::settings::Settings,
    listen: &std::cell::RefCell<Vec<ListenChoice>>,
) {
    use std::net::{IpAddr, Ipv4Addr};
    let mut labels: Vec<slint::SharedString> = vec!["This computer only".into()];
    let mut choices = vec![ListenChoice {
        ip: Ipv4Addr::LOCALHOST.into(),
        interface: None,
    }];
    // Networks for containers and virtual machines stay out, unless picked
    let interfaces = mcp::network::interfaces()
        .into_iter()
        .filter(|i| i.usable || settings.mcp_interface.as_deref() == Some(i.name.as_str()));
    for i in interfaces {
        labels.push(format!("{} ({})", i.name, i.ip).into());
        choices.push(ListenChoice {
            ip: i.ip,
            interface: Some(i.name),
        });
    }
    labels.push("All networks".into());
    choices.push(ListenChoice {
        ip: Ipv4Addr::UNSPECIFIED.into(),
        interface: None,
    });

    let address = mcp::agents::listen_address(settings);
    let saved: Option<IpAddr> = mcp::network::split_address(&address).map(|a| a.ip());
    let picked = choices
        .iter()
        .position(|c| settings.mcp_interface.is_some() && c.interface == settings.mcp_interface);
    let picked = picked.or_else(|| choices.iter().position(|c| Some(c.ip) == saved));
    let picked = picked.unwrap_or_else(|| {
        // An address no interface has now, e.g. from an unplugged network
        let ip = saved
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| address.clone());
        labels.push(format!("{ip} (not available)").into());
        choices.push(ListenChoice {
            ip: saved.unwrap_or(Ipv4Addr::LOCALHOST.into()),
            interface: settings.mcp_interface.clone(),
        });
        choices.len() - 1
    });

    ui.set_agents_listen_options(std::rc::Rc::new(slint::VecModel::from(labels)).into());
    ui.set_agents_listen(picked as i32);
    ui.set_agents_listen_local(choices[picked].ip.is_loopback());
    let port: slint::SharedString = saved_port(settings).to_string().into();
    ui.set_agents_port(port.clone());
    ui.set_agents_edit_port(port);
    *listen.borrow_mut() = choices;
}

/// The port of the saved network address
fn saved_port(settings: &radiotrope_app::data::settings::Settings) -> u16 {
    mcp::network::split_address(&settings.mcp_address)
        .map(|a| a.port())
        .unwrap_or_else(default_port)
}

fn default_port() -> u16 {
    radiotrope_app::config::mcp::DEFAULT_ADDRESS
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8765)
}

/// What the Agents dialog says when a new token can't be made
fn token_error(e: &std::io::Error) -> String {
    format!(
        "Can't make a new token: {}",
        radiotrope_app::text::io_reason(e)
    )
}

/// The line under THIS COMPUTER: the agents connected now, or that none is
fn connected_line(rows: &[AgentRow]) -> String {
    let names: Vec<&str> = rows
        .iter()
        .filter(|a| a.connected)
        .map(|a| a.name.as_str())
        .collect();
    if names.is_empty() {
        "No agent connected yet".into()
    } else {
        format!("Connected: {}", names.join(", "))
    }
}

/// The saved address is this computer's own
fn listens_locally(settings: &radiotrope_app::data::settings::Settings) -> bool {
    settings.mcp_interface.is_none()
        && mcp::network::split_address(&settings.mcp_address).is_none_or(|a| a.ip().is_loopback())
}

/// Authentication after "Listen on" changes: moving from this computer to
/// a network with no token turns the token on, unless None was picked by
/// hand
fn safer_auth(
    auth: radiotrope_app::data::settings::McpAuth,
    was_local: bool,
    now: std::net::IpAddr,
    picked: bool,
) -> radiotrope_app::data::settings::McpAuth {
    use radiotrope_app::data::settings::McpAuth;
    if auth == McpAuth::None && was_local && !now.is_loopback() && !picked {
        McpAuth::Token
    } else {
        auth
    }
}

fn save_agent_settings(change: impl FnOnce(&mut radiotrope_app::data::settings::Settings)) {
    crate::window_settings::update_settings(change);
}

#[cfg(test)]
mod tests {
    use super::*;
    use radiotrope_app::data::settings::McpAuth;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn token_failure_is_said_in_plain_words() {
        let e = std::io::Error::from_raw_os_error(13);
        assert_eq!(token_error(&e), "Can't make a new token: permission denied");
    }

    #[test]
    fn connected_line_names_connected_agents() {
        let row = |name: &str, connected: bool| AgentRow {
            name: name.into(),
            connected,
            ..Default::default()
        };
        assert_eq!(connected_line(&[]), "No agent connected yet");
        assert_eq!(
            connected_line(&[row("Claude Code", true), row("Codex", false)]),
            "Connected: Claude Code"
        );
    }

    #[test]
    fn leaving_this_computer_turns_the_token_on_once() {
        let lan: IpAddr = Ipv4Addr::new(192, 168, 1, 20).into();
        let local: IpAddr = Ipv4Addr::LOCALHOST.into();
        assert_eq!(safer_auth(McpAuth::None, true, lan, false), McpAuth::Token);
        // None picked by hand, already on a network, or staying local
        assert_eq!(safer_auth(McpAuth::None, true, lan, true), McpAuth::None);
        assert_eq!(safer_auth(McpAuth::None, false, lan, false), McpAuth::None);
        assert_eq!(safer_auth(McpAuth::None, true, local, false), McpAuth::None);
        assert_eq!(safer_auth(McpAuth::Token, true, lan, false), McpAuth::Token);
    }
}
