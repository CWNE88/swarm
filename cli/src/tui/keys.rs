//! The terminal UI's keys: what each does for what's showing, the
//! forms typed into, and the line being written.

use super::*;

impl App {
    pub(super) async fn handle_key(&mut self, mut key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }
        self.last_key = Instant::now();
        if let Some((typed, why)) = self.locked.as_mut() {
            match key.code {
                KeyCode::Char(c) => typed.push(c),
                KeyCode::Backspace => {
                    typed.pop();
                }
                KeyCode::Enter => {
                    let pass = std::mem::take(typed);
                    *why = "checking…".to_string();
                    let opens = tokio::task::spawn_blocking(move || swarm_app::launch::is_passphrase(&swarm_app::launch::identity_path(), &pass)).await.unwrap_or(false);
                    if opens {
                        self.locked = None;
                    } else if let Some((_, why)) = self.locked.as_mut() {
                        *why = "that isn't the passphrase".to_string();
                    }
                }
                _ => {}
            }
            return;
        }
        if self.focus == Focus::Nav {
            self.input_back = 0;
        }

        if let Some(prompt) = self.calls.incoming() {
            let choices = ring_choices(prompt.video);
            let note = match key.code {
                KeyCode::Left => {
                    self.ring_choice = self.ring_choice.saturating_sub(1);
                    None
                }
                KeyCode::Right => {
                    self.ring_choice = (self.ring_choice + 1).min(choices.len() - 1);
                    None
                }
                KeyCode::Enter => match choices[self.ring_choice.min(choices.len() - 1)] {
                    RingChoice::Video => self.calls.answer(&self.node, &self.sidebar, true),
                    RingChoice::Audio => self.calls.answer(&self.node, &self.sidebar, false),
                    RingChoice::Reject => self.calls.reject(),
                },
                KeyCode::Char('a') | KeyCode::Char('A') => self.calls.answer(&self.node, &self.sidebar, true),
                KeyCode::Char('s') | KeyCode::Char('S') => self.calls.answer(&self.node, &self.sidebar, false),
                KeyCode::Char('r') | KeyCode::Char('R') | KeyCode::Esc => self.calls.reject(),
                _ => None,
            };
            self.note(note);
            return;
        }

        if self.control().asking.is_some() {
            let note = match key.code {
                KeyCode::Char('a') | KeyCode::Char('A') => self.calls.answer_control(&self.node, &self.sidebar, true),
                KeyCode::Char('r') | KeyCode::Char('R') | KeyCode::Esc => self.calls.answer_control(&self.node, &self.sidebar, false),
                _ => return,
            };
            self.note(note);
            return;
        }
        if self.control().controller.is_some() && matches!(key.code, KeyCode::Char('x') | KeyCode::Char('X')) {
            let note = self.calls.stop_control();
            self.note(note);
            return;
        }
        if let Some(prompt) = self.offer_prompt.take() {
            let id = prompt.transfer.id;
            match key.code {
                KeyCode::Char('a') | KeyCode::Char('A') => self.open_save_browser(id),
                KeyCode::Char('d') | KeyCode::Char('D') => self.say(actions::decline_transfer(&self.node, &prompt.transfer)),
                KeyCode::Char('l') | KeyCode::Char('L') | KeyCode::Esc => self.say(actions::transfer_left(&prompt.transfer)),
                _ => self.offer_prompt = Some(prompt),
            }
            return;
        }
        if let Some((id, what)) = self.channel_confirm.take() {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                let note = commands::confirm(&self.node, &id, what);
                self.note(Some(note));
                self.refresh_rows();
            } else {
                self.set_status(StatusKind::Info, "cancelled");
            }
            return;
        }
        if let Some((browser, purpose)) = &mut self.browser {
            match browser.handle_key(key) {
                crate::browser::Outcome::Browsing => {}
                crate::browser::Outcome::Cancelled => {
                    if let Browsing::SaveFor(_) = purpose {
                        self.set_status(StatusKind::Info, "not accepted yet — t: transfers to come back to it");
                    }
                    self.browser = None;
                }
                crate::browser::Outcome::File(path) if matches!(purpose, Browsing::Avatar) => {
                    self.browser = None;
                    if let Some(dir) = path.parent() {
                        let _ = swarm_core::settings::set(&paths().settings(), "avatar_from", &dir.to_string_lossy());
                    }
                    let made = tokio::task::block_in_place(|| swarm_app::avatar::from_file(&path));
                    self.set_avatar(made);
                }
                crate::browser::Outcome::File(path) => {
                    let Browsing::SendTo(peer) = purpose else { return };
                    let peer = peer.clone();
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    let estimate = self.transfer_estimate(&peer, size);
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    self.set_status(
                        StatusKind::Warning,
                        format!("send {name} ({}) to {}{estimate}? y/n", crate::browser::human_size(size), self.node.label_of(&peer)),
                    );
                    self.confirm_send = Some((peer, path.clone(), size));
                    let _ = swarm_core::settings::set(&paths().settings(), "send_from", &path.parent().unwrap_or(&path).to_string_lossy());
                    self.browser = None;
                }
                crate::browser::Outcome::Folder(folder) => {
                    let Browsing::SaveFor(id) = purpose else { return };
                    let id = *id;
                    self.browser = None;
                    match self.node.accept_transfer(&id, &folder) {
                        Ok(()) => {
                            let _ = swarm_core::settings::set(&paths().settings(), "downloads", &folder.to_string_lossy());
                            self.set_status(StatusKind::Success, format!("accepted — saving in {} — t: transfers", folder.display()));
                        }
                        Err(e) => self.set_status(StatusKind::Error, format!("couldn't accept: {e:#}")),
                    }
                }
            }
            return;
        }
        if let Some((peer, path, _)) = self.confirm_send.take() {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.set_status(StatusKind::Info, format!("offering {name} to {}...", self.node.label_of(&peer)));
                self.jobs.offer(&self.node, peer, path);
            } else {
                self.set_status(StatusKind::Info, "not sent");
            }
            return;
        }
        if self.focus == Focus::Nav && self.buttons_take_keys() {
            let buttons = self.buttons();
            let last = buttons.len().saturating_sub(1);
            match key.code {
                KeyCode::Left if !buttons.is_empty() => {
                    self.button_choice = Some(self.button_choice.map_or(0, |i| i.saturating_sub(1)).min(last));
                    return;
                }
                KeyCode::Right if !buttons.is_empty() => {
                    self.button_choice = Some(self.button_choice.map_or(0, |i| i + 1).min(last));
                    return;
                }
                KeyCode::Enter => {
                    if let Some(button) = self.button_choice.and_then(|i| buttons.get(i)) {
                        key = KeyEvent::new(button.key, KeyModifiers::NONE);
                    }
                }
                _ => {}
            }
        }
        self.button_choice = None;
        if self.mode == MainMode::Transfers && self.focus == Focus::Nav {
            let all = self.shown_transfers();
            let ids: Vec<_> = all.iter().map(|t| t.transfer.id).collect();
            let selected = all.get(picked_at(&ids, self.transfer_selected)).cloned();
            match key.code {
                KeyCode::Tab | KeyCode::BackTab => {
                    self.in_transfers = !self.in_transfers;
                    return;
                }
                KeyCode::Up | KeyCode::Char('k') if self.in_transfers => {
                    self.transfer_selected = moved_to(&ids, self.transfer_selected, -1);
                    return;
                }
                KeyCode::Down | KeyCode::Char('j') if self.in_transfers => {
                    self.transfer_selected = moved_to(&ids, self.transfer_selected, 1);
                    return;
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    if let Some(t) = selected.filter(is_open_offer) {
                        self.open_save_browser(t.transfer.id);
                    }
                    return;
                }
                KeyCode::Char('d') | KeyCode::Char('D') => {
                    if let Some(t) = selected.filter(is_open_offer) {
                        self.say(actions::decline_transfer(&self.node, &t.transfer));
                    }
                    return;
                }
                KeyCode::Char('x') | KeyCode::Char('X') => {
                    if let Some(t) = selected.filter(|t| !t.transfer.state.is_finished()) {
                        self.say(actions::cancel_transfer(&self.node, &t.transfer));
                    }
                    return;
                }
                KeyCode::Char('l') => {
                    if let Some(t) = selected.filter(|t| t.transfer.state.is_finished()) {
                        self.say(actions::clear_transfers(&self.node, &[t.transfer]));
                    }
                    return;
                }
                KeyCode::Char('L') => {
                    let finished: Vec<_> = all.into_iter().map(|t| t.transfer).filter(|t| t.state.is_finished()).collect();
                    if !finished.is_empty() {
                        self.say(actions::clear_transfers(&self.node, &finished));
                    }
                    return;
                }
                _ => {}
            }
        }
        if key.code == KeyCode::Esc && self.device_code.is_some() && self.focus == Focus::Nav && matches!(self.sidebar.rows.get(self.selected), Some(SidebarEntry::You)) {
            self.device_code = None;
            return;
        }
        if matches!(self.mode, MainMode::Peers | MainMode::View) && self.focus == Focus::Nav && self.calls.preview().is_none() {
            if let Some(SidebarEntry::Device(chosen)) = self.sidebar.rows.get(self.selected) {
                let device = self.node.devices().devices.into_iter().find(|d| d.key == chosen.key);
                match (key.code, device) {
                    (KeyCode::Char('n' | 'N'), Some(device)) if device.this => {
                        self.device_input = Some(DeviceInput::Rename);
                        self.input = swarm_app::devices::name_here(&self.node);
                        self.focus = Focus::Input;
                        return;
                    }
                    (KeyCode::Char('l' | 'L'), Some(device)) if self.node.devices().linked() => {
                        self.say(actions::allow_relay(&self.node, &device, !device.relay));
                        return;
                    }
                    (KeyCode::Char('g' | 'G'), Some(device)) if device.this => {
                        self.say(actions::receive_only(&self.node, !device.receive_only));
                        return;
                    }
                    (KeyCode::Char('r' | 'R'), Some(device)) if device.this => {
                        self.device_input = Some(DeviceInput::ReceiveLimit);
                        self.input = actions::receive_limit_text(&self.node);
                        self.focus = Focus::Input;
                        return;
                    }
                    (KeyCode::Char('z' | 'Z'), Some(device)) if device.this => {
                        self.device_input = Some(DeviceInput::BandwidthLimit);
                        self.input = actions::bandwidth_limit_text(&self.node);
                        self.focus = Focus::Input;
                        return;
                    }
                    (KeyCode::Char(key @ ('c' | 'C' | 'v' | 'V')), Some(device)) if !device.this && self.calls.active().is_none() => {
                        let note = self.calls.place_device(&self.node, &device, matches!(key, 'v' | 'V'));
                        self.note(Some(note));
                        return;
                    }
                    (KeyCode::Char('o' | 'O'), Some(device)) if !device.this => {
                        self.say(actions::device_answer(&self.node, &device, None));
                        return;
                    }
                    (KeyCode::Char('a' | 'A'), Some(device)) if !device.this => {
                        self.say(actions::device_control(&self.node, &device, None));
                        return;
                    }
                    (KeyCode::Char('i' | 'I'), Some(device)) if !device.this => {
                        self.device_input = Some(DeviceInput::Address(device.key));
                        self.input = device.typed.clone();
                        self.focus = Focus::Input;
                        return;
                    }
                    (KeyCode::Char('d' | 'D'), Some(device)) if !device.this => {
                        self.pending_device_remove = Some(device.key);
                        self.set_status(StatusKind::Warning, format!("{} — y to confirm, any other key cancels", swarm_app::text::capitalised(&actions::remove_device_question(&device))));
                        return;
                    }
                    _ => {}
                }
            }
        }
        if self.mode == MainMode::Peers && self.focus == Focus::Nav && self.calls.preview().is_none() && matches!(self.sidebar.rows.get(self.selected), Some(SidebarEntry::You)) {
            match key.code {
                KeyCode::Char('n' | 'N') => {
                    self.name_prompt = Some(prefs::Setting::Name);
                    self.input = my_name().unwrap_or_default();
                    self.focus = Focus::Input;
                    return;
                }
                KeyCode::Char('i' | 'I') => {
                    let manual = self.node.manual_address().text();
                    self.input = if manual.is_empty() {
                        swarm_app::addrs::seen_from_outside(&self.node, &self.sidebar.peer_names()).first().map(|(ip, _)| ip.to_string()).unwrap_or_default()
                    } else {
                        manual
                    };
                    self.name_prompt = Some(prefs::Setting::ManualAddress);
                    self.focus = Focus::Input;
                    return;
                }
                KeyCode::Char('u' | 'U') => {
                    self.name_prompt = Some(prefs::Setting::Status);
                    self.input = swarm_app::avatar::status(&self.identity.public_key_bytes(), true);
                    self.focus = Focus::Input;
                    return;
                }
                KeyCode::Char('f' | 'F') => {
                    self.choose_picture_file();
                    return;
                }
                KeyCode::Char('e' | 'E') => {
                    if self.calls.active().is_some_and(|c| c.video.is_some()) {
                        self.set_status(StatusKind::Error, "the camera's in the call — take your picture after it");
                    } else {
                        self.open_preview();
                        self.taking_picture = true;
                        self.set_status(StatusKind::Info, "Enter: take it   Esc: not now");
                    }
                    return;
                }
                KeyCode::Char('x' | 'X') => {
                    self.set_avatar(Ok(Vec::new()));
                    return;
                }
                KeyCode::Char('d' | 'D') => {
                    let on = !self.node.discoverable().on;
                    self.say(actions::set_discoverable(&self.node, on));
                    return;
                }
                KeyCode::Char('b' | 'B') => {
                    match actions::blocked(&self.node).first() {
                        Some(first) => {
                            self.name_prompt = Some(prefs::Setting::Unblock);
                            self.input = first.label();
                            self.focus = Focus::Input;
                        }
                        None => self.set_status(StatusKind::Info, "nobody's blocked"),
                    }
                    return;
                }
                _ => {}
            }
        }
        if self.mode == MainMode::Help && self.focus == Focus::Nav {
            let manual = swarm_app::manual::get();
            let page = self.help_height.get().saturating_sub(2).max(1);
            let scroll = self.help_scroll.get();
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.help_open(manual.step(self.help_at, -1)),
                KeyCode::Down | KeyCode::Char('j') => self.help_open(manual.step(self.help_at, 1)),
                KeyCode::PageUp | KeyCode::Char('b') => self.help_scroll.set(scroll.saturating_sub(page)),
                KeyCode::PageDown | KeyCode::Char(' ') => self.help_scroll.set(scroll.saturating_add(page)),
                KeyCode::Home => self.help_scroll.set(0),
                KeyCode::End => self.help_scroll.set(u16::MAX),
                KeyCode::Char(c) if !"pPtTsSiIqQ".contains(c) => {}
                _ => return self.handle_nav_key(key).await,
            }
            return;
        }
        if self.mode == MainMode::Settings && self.focus == Focus::Nav && self.calls.preview().is_none() {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.setting_selected = self.setting_selected.saturating_sub(1);
                    return;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.setting_selected = (self.setting_selected + 1).min(prefs::SETTINGS.len() - 1);
                    return;
                }
                KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right => {
                    let setting = prefs::SETTINGS[self.setting_selected];
                    if setting == prefs::Setting::Preview {
                        self.open_preview();
                    } else if setting == prefs::Setting::SoundFile {
                        self.name_prompt = Some(setting);
                        self.input = swarm_core::settings::get(&paths().settings(), swarm_app::sound::SETTING).unwrap_or_default();
                        self.focus = Focus::Input;
                    } else if setting == prefs::Setting::Ringtone {
                        self.name_prompt = Some(setting);
                        self.input = swarm_core::settings::get(&paths().settings(), swarm_app::ringtone::SETTING).unwrap_or_default();
                        self.focus = Focus::Input;
                    } else if let Err(e) = self.prefs.cycle(&paths().settings(), setting) {
                        self.set_status(StatusKind::Error, format!("couldn't save that: {e}"));
                    } else if setting == prefs::Setting::Order {
                        self.refresh_rows();
                    }
                    return;
                }
                _ => {}
            }
        }
        if let (Some(_), Some(preview)) = (self.calls.preview(), self.preview.as_mut()) {
            match key.code {
                KeyCode::Esc | KeyCode::Char('w') | KeyCode::Char('W') | KeyCode::Char('q') | KeyCode::Char('Q') => {
                    self.calls.close_preview();
                    self.preview = None;
                    self.taking_picture = false;
                    self.set_status(StatusKind::Info, "camera off");
                }
                KeyCode::Enter | KeyCode::Char(' ') if self.taking_picture => {
                    let picture = self.calls.preview().and_then(|p| p.as_ref().ok()).and_then(|c| c.latest());
                    let Some(picture) = picture else {
                        self.set_status(StatusKind::Error, "the camera hasn't shown a picture yet");
                        return;
                    };
                    let made = tokio::task::block_in_place(|| swarm_app::avatar::from_picture(&picture));
                    self.calls.close_preview();
                    self.preview = None;
                    self.taking_picture = false;
                    self.set_avatar(made);
                }
                KeyCode::Char('m') | KeyCode::Char('M') => preview.mode = preview.mode.next(),
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Char('w')
            && self.focus == Focus::Nav
            && self.peer_form.is_none()
            && self.channel_form.is_none()
            && self.calls.active().is_none()
        {
            self.mode = MainMode::Settings;
            self.setting_selected = prefs::SETTINGS.iter().position(|s| *s == prefs::Setting::Preview).unwrap_or(0);
            self.open_preview();
            return;
        }

        if self.mode == MainMode::View
            && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
            && matches!(self.sidebar.rows.get(self.selected), Some(SidebarEntry::Peer(_) | SidebarEntry::Channel(_)))
        {
            let (height, overflow) = self.chat_view.get();
            let page = height.saturating_sub(1).max(1);
            self.chat_scroll = match key.code {
                KeyCode::PageUp => self.chat_scroll.saturating_add(page).min(overflow),
                _ => self.chat_scroll.saturating_sub(page),
            };
            return;
        }
        if key.code == KeyCode::Tab && self.typing_a_message() && self.complete_name() {
            return;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
            && self.mode == MainMode::View
            && self.post_compose.is_none()
            && self.selected_peer().is_some()
        {
            self.peer_tab = if self.peer_tab == PeerTab::Chat { PeerTab::Posts } else { PeerTab::Chat };
            if self.peer_tab == PeerTab::Posts {
                self.focus = Focus::Nav;
            }
            self.chat_scroll = 0;
            self.refresh_unread();
            return;
        }
        if self.focus == Focus::Input {
            self.handle_input_key(key).await;
            return;
        }
        self.handle_nav_key(key).await;
    }

    pub(super) async fn handle_nav_key(&mut self, key: KeyEvent) {
        if let Some(request) = self.pending_block.take() {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.say(actions::block_request(&request));
                self.refresh_rows();
            } else {
                self.set_status(StatusKind::Info, "cancelled");
            }
            return;
        }
        if let Some(request) = self.selected_request().cloned() {
            if matches!(self.mode, MainMode::View | MainMode::Peers) {
                match key.code {
                    KeyCode::Char('a') | KeyCode::Char('A') | KeyCode::Enter => {
                        self.start_accept_form(request);
                        return;
                    }
                    KeyCode::Char('x') | KeyCode::Char('X') => {
                        self.say(actions::dismiss_request(&request));
                        self.refresh_rows();
                        return;
                    }
                    KeyCode::Char('b') | KeyCode::Char('B') => {
                        self.set_status(StatusKind::Warning, format!("{} y to confirm, any other key cancels", actions::block_question(&request)));
                        self.pending_block = Some(request);
                        return;
                    }
                    _ => {}
                }
            }
        }
        if let Some(device_key) = self.pending_device_remove.take() {
            match self.node.devices().devices.into_iter().find(|d| d.key == device_key) {
                Some(device) if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) => self.say(actions::remove_device(&self.node, &device)),
                _ => self.set_status(StatusKind::Info, "cancelled"),
            }
            return;
        }
        if let Some(target) = self.pending_remove.take() {
            match key.code {
                KeyCode::Char('d' | 'D') => self.say(actions::remove_peer(&self.node, &target, false)),
                KeyCode::Char('b' | 'B') => self.say(actions::remove_peer(&self.node, &target, true)),
                _ => self.set_status(StatusKind::Info, "cancelled"),
            }
            self.refresh_rows();
            return;
        }

        if let Some(typed) = self.removing.take() {
            self.on_removing_key(key, typed);
            return;
        }
        let in_video_call = self.calls.active().is_some_and(|c| c.video.is_some());
        if !in_video_call {
            self.video_only = false;
        }
        if self.video_only {
            match key.code {
                KeyCode::Esc | KeyCode::Char('v') | KeyCode::Char('V') => self.video_only = false,
                KeyCode::Char('s') | KeyCode::Char('S') => self.hide_self = !self.hide_self,
                KeyCode::Char('m') | KeyCode::Char('M') => self.calls.toggle_mute(),
                KeyCode::Char('w') | KeyCode::Char('W') => {
                    let note = self.calls.toggle_camera(&self.node, &self.sidebar);
                    self.note(note);
                }
                KeyCode::Char('e') | KeyCode::Char('E') => {
                    let note = self.calls.toggle_screen(&self.node, &self.sidebar);
                    self.note(note);
                }
                KeyCode::Char('h') | KeyCode::Char('H') => {
                    let note = self.calls.hang_up(&self.node);
                    self.video_only = false;
                    self.note(note);
                }
                _ => {}
            }
            return;
        }
        if matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V')) && in_video_call {
            self.video_only = true;
            self.video_only_since = Instant::now();
            return;
        }
        if (key.code == KeyCode::Char('h') || key.code == KeyCode::Char('H')) && (self.calls.active().is_some() || self.calls.outgoing().is_some()) {
            let note = self.calls.hang_up(&self.node);
            self.note(note);
            return;
        }
        if matches!(key.code, KeyCode::Char('m') | KeyCode::Char('M')) && self.calls.active().is_some() {
            self.calls.toggle_mute();
            return;
        }
        if matches!(key.code, KeyCode::Char('w') | KeyCode::Char('W')) && self.calls.active().is_some() {
            let note = self.calls.toggle_camera(&self.node, &self.sidebar);
            self.note(note);
            return;
        }
        if matches!(key.code, KeyCode::Char('e') | KeyCode::Char('E')) && self.calls.active().is_some() {
            let note = self.calls.toggle_screen(&self.node, &self.sidebar);
            self.note(note);
            return;
        }
        if key.code == KeyCode::Char('K') && self.calls.active().is_some_and(|c| c.in_conference()) {
            self.removing = Some(String::new());
            self.ask_whom_to_remove("");
            return;
        }
        if key.code == KeyCode::Char('c') && matches!(self.mode, MainMode::View | MainMode::Peers) && self.calls.active().is_some() && self.selected_channel().is_none() {
            if let Some(row) = self.selected_peer().cloned().filter(|row| self.calls.can_add(row)) {
                let note = self.calls.add(&self.node, &self.sidebar, &row);
                self.note(Some(note));
            }
            return;
        }
        if matches!(key.code, KeyCode::Char('c' | 'C')) && matches!(self.mode, MainMode::View | MainMode::Peers) && self.selected_channel().is_some() {
            let id = self.selected_channel().cloned().expect("checked");
            let name = match self.sidebar.rows.get(self.selected) {
                Some(SidebarEntry::Channel(row)) => row.name.clone(),
                _ => String::new(),
            };
            let note = self.calls.call_group(&self.node, &self.sidebar, &name, &swarm_app::calls::group_contacts(&self.node, &self.sidebar, &id));
            self.note(Some(note));
            return;
        }
        if matches!(key.code, KeyCode::Char('c' | 'C' | 'v' | 'V')) && matches!(self.mode, MainMode::View | MainMode::Peers) && !self.calls.busy() && self.selected_peer().is_some() {
            if let Some(row) = self.selected_peer().cloned() {
                let note = self.calls.place(&self.node, &row, matches!(key.code, KeyCode::Char('v' | 'V')));
                self.note(Some(note));
            }
            return;
        }

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected > 0 {
                    self.selected -= 1;
                    self.chat_scroll = 0;
                    self.peer_tab = PeerTab::Chat;
                    self.refresh_unread();
                    self.fetch_timeline(false);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.sidebar.rows.len() {
                    self.selected += 1;
                    self.chat_scroll = 0;
                    self.peer_tab = PeerTab::Chat;
                    self.refresh_unread();
                    self.fetch_timeline(false);
                }
            }
            KeyCode::Enter if matches!(self.mode, MainMode::View | MainMode::Peers) && matches!(self.sidebar.rows.get(self.selected), Some(SidebarEntry::Heading(_))) => {
                if let Some(SidebarEntry::Heading(heading)) = self.sidebar.rows.get(self.selected) {
                    let section = heading.section;
                    if let Err(e) = self.sidebar.toggle(section, &mut self.prefs, &paths().settings()) {
                        self.set_status(StatusKind::Error, format!("couldn't save the list's headings: {e}"));
                    }
                }
            }
            KeyCode::Enter if self.mode == MainMode::View => match self.sidebar.rows.get(self.selected) {
                Some(SidebarEntry::Peer(_)) | Some(SidebarEntry::Channel(_)) => {
                    self.focus = Focus::Input;
                    self.input.clear();
                }
                Some(SidebarEntry::You) => {
                    self.post_compose = Some(PostCompose::Text);
                    self.focus = Focus::Input;
                    self.input.clear();
                }
                Some(SidebarEntry::Request(request)) => {
                    let request = request.clone();
                    self.start_accept_form(request);
                }
                Some(SidebarEntry::Followed(_)) | Some(SidebarEntry::Heading(_)) | Some(SidebarEntry::Device(_)) | Some(SidebarEntry::Add(_)) | None => {}
            },
            KeyCode::Enter if self.mode == MainMode::Peers => {
                self.mode = MainMode::View;
                self.peer_tab = PeerTab::Chat;
                let writes = matches!(self.sidebar.rows.get(self.selected), Some(SidebarEntry::Peer(_) | SidebarEntry::Channel(_)));
                if writes {
                    self.focus = Focus::Input;
                    self.input.clear();
                }
                self.refresh_unread();
            }
            KeyCode::Enter if self.mode == MainMode::Peers && matches!(self.sidebar.rows.get(self.selected), Some(SidebarEntry::Add(_))) => {
                if let Some(SidebarEntry::Add(adding)) = self.sidebar.rows.get(self.selected) {
                    let adding = *adding;
                    self.start_adding(adding);
                }
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => self.should_quit = true,
            KeyCode::Char('1') | KeyCode::Char('p') | KeyCode::Char('P') => self.mode = MainMode::Peers,
            KeyCode::Char('2') => self.mode = MainMode::View,
            KeyCode::Char('i' | 'I') if self.mode == MainMode::Peers && self.selected_peer().is_some() => {
                if let Some(row) = self.selected_peer().cloned() {
                    self.device_input = Some(DeviceInput::ContactAddress(row.public_key));
                    self.input = row.address.clone();
                    self.focus = Focus::Input;
                }
            }
            KeyCode::Char('i') | KeyCode::Char('I') => self.mode = MainMode::Peers,
            KeyCode::Char('3') | KeyCode::Char('t') | KeyCode::Char('T') => {
                self.mode = MainMode::Transfers;
                self.in_transfers = true;
            }
            KeyCode::Char('4') | KeyCode::Char('s') | KeyCode::Char('S') => self.mode = MainMode::Settings,
            KeyCode::Char('5') | KeyCode::Char('?') | KeyCode::F(1) => self.mode = MainMode::Help,
            KeyCode::Char('x') | KeyCode::Char('X') if self.mode == MainMode::View && (self.selected_peer().is_some() || self.selected_channel().is_some()) => {
                self.maximised = !self.maximised
            }
            KeyCode::Esc if self.maximised || self.mode != MainMode::View => {
                self.maximised = false;
                self.mode = MainMode::View;
            }
            KeyCode::Char('l' | 'L') if self.mode == MainMode::Peers && self.selected_channel().is_some() => {
                let id = self.selected_channel().cloned().expect("checked");
                self.channel_command(&id, "/leave");
            }
            KeyCode::Char('g' | 'G') if matches!(self.mode, MainMode::View | MainMode::Peers) && self.selected_channel().is_some() => self.start_adding_to_group(),
            KeyCode::Char('g' | 'G') if matches!(self.mode, MainMode::View | MainMode::Peers) && self.selected_peer().is_some() => {
                let handle = self.selected_peer().map(|row| row.handle.clone()).expect("checked");
                self.start_group_with(handle);
            }
            KeyCode::Char('o') | KeyCode::Char('O') if self.mode == MainMode::Peers => {
                if let Some(row) = self.selected_peer().cloned() {
                    self.say(actions::cycle_auto_answer(&row));
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') if self.mode == MainMode::Peers => {
                if let Some(row) = self.selected_peer().cloned() {
                    self.say(actions::set_auto_control(&row, None));
                    self.refresh_rows();
                }
            }
            KeyCode::Char('d') | KeyCode::Char('D') if self.mode == MainMode::View && self.peer_tab == PeerTab::Chat => {
                if let Some(row) = self.selected_peer().cloned() {
                    self.say(actions::next_timer(&self.node, &row));
                }
            }
            KeyCode::Char('f') | KeyCode::Char('F') if matches!(self.mode, MainMode::View | MainMode::Peers) => {
                if let Some(row) = self.selected_peer().cloned() {
                    let start = swarm_core::settings::get(&paths().settings(), "send_from").map(PathBuf::from).unwrap_or_else(crate::browser::home);
                    self.browser = Some((crate::browser::Browser::new(crate::browser::Purpose::File, &start), Browsing::SendTo(row.handle)));
                }
            }
            KeyCode::Char('u') | KeyCode::Char('U') if self.mode == MainMode::Peers => {
                if let Some(row) = self.selected_peer().cloned() {
                    self.peer_form = Some(PeerForm {
                        kind: PeerFormKind::Update,
                        target: Some(row.handle.clone()),
                        step: 0,
                        name: row.label.clone(),
                        address: row.address.clone(),
                        pubkey: STANDARD.encode(row.public_key),
                        cover: cover_setting_text(&row),
                        request: None,
                    });
                    self.input = row.label.clone();
                    self.focus = Focus::Input;
                }
            }
            KeyCode::Char('d') | KeyCode::Char('D') if self.mode == MainMode::Peers => {
                if let Some(row) = self.selected_peer().filter(|row| row.removed_us).cloned() {
                    self.say(actions::remove_peer(&self.node, &row.handle, false));
                    self.refresh_rows();
                } else if let Some((name, shown, asked)) = self.selected_peer().map(|row| (row.handle.clone(), row.label.clone(), row.asked)) {
                    self.pending_remove = Some(name.clone());
                    let (keep, what) = (actions::remove_peer_question(&shown, asked), if asked { "cancel the request" } else { "delete" });
                    self.set_status(StatusKind::Warning, format!("{} d: {what}, b: block, any other key keeps them", swarm_app::text::capitalised(&keep)));
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') if self.mode == MainMode::Peers && self.selected_peer().is_some() => {
                if let Some(row) = self.selected_peer().cloned() {
                    self.input = swarm_core::settings::receive_to(&paths().settings(), &row.public_key).map(|f| f.display().to_string()).unwrap_or_default();
                    self.receive_prompt = Some(row.clone());
                    self.focus = Focus::Input;
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') if self.mode == MainMode::View => {
                self.post_compose = Some(PostCompose::Text);
                self.focus = Focus::Input;
                self.input.clear();
            }
            KeyCode::Char('r') | KeyCode::Char('R') if matches!(self.mode, MainMode::View | MainMode::Peers) && self.selected_followed().is_some() => {
                self.fetch_followed(true);
            }
            KeyCode::Char('x') | KeyCode::Char('X') if matches!(self.mode, MainMode::View | MainMode::Peers) && self.selected_followed().is_some() => {
                let row = self.selected_followed().cloned().expect("checked");
                self.say(actions::unfollow(&self.node, &row));
                self.refresh_rows();
            }
            KeyCode::Char('r') | KeyCode::Char('R') if self.mode == MainMode::View => {
                self.fetch_timeline(true);
                if let Some(id) = self.selected_channel().cloned() {
                    self.node.resync_channel(&id);
                    self.set_status(StatusKind::Info, "catching up with everyone in it who's connected");
                }
            }
            _ => {}
        }
    }

    pub(super) async fn handle_input_key(&mut self, key: KeyEvent) {
        if let Some(what) = self.device_input {
            match key.code {
                KeyCode::Esc => {
                    self.device_input = None;
                    self.focus = Focus::Nav;
                    self.input.clear();
                }
                KeyCode::Enter => {
                    self.device_input = None;
                    self.focus = Focus::Nav;
                    let typed = std::mem::take(&mut self.input).trim().to_string();
                    if typed.is_empty() && !matches!(what, DeviceInput::Address(_) | DeviceInput::BandwidthLimit | DeviceInput::ReceiveLimit) {
                        return;
                    }
                    match what {
                        DeviceInput::Rename => self.say(actions::rename_device(&self.node, &typed)),
                        DeviceInput::BandwidthLimit => self.say(actions::bandwidth_limit(&self.node, &typed)),
                        DeviceInput::ReceiveLimit => self.say(actions::receive_limit(&self.node, &typed)),
                        DeviceInput::Address(key) => {
                            if let Some(device) = self.node.devices().devices.into_iter().find(|d| d.key == key) {
                                self.say(actions::set_device_address(&self.node, &device, &typed));
                            }
                        }
                        DeviceInput::ContactAddress(key) => {
                            if let Some(row) = self.sidebar.rows.iter().find_map(|r| match r {
                                SidebarEntry::Peer(row) if row.public_key == key => Some(row.clone()),
                                _ => None,
                            }) {
                                self.say(actions::set_contact_address(&self.node, &row, &typed));
                                self.refresh_rows();
                            }
                        }
                    }
                }
                _ => {
                    lineedit::edit(&mut self.input, &mut self.input_back, key);
                }
            }
            return;
        }
        if let Some(prompt) = self.name_prompt {
            match key.code {
                KeyCode::Esc => {
                    self.name_prompt = None;
                    self.focus = Focus::Nav;
                    self.input.clear();
                }
                KeyCode::Enter if prompt == prefs::Setting::SoundFile => {
                    self.name_prompt = None;
                    self.focus = Focus::Nav;
                    let path = std::mem::take(&mut self.input).trim().to_string();
                    let settings = paths().settings();
                    if path.is_empty() {
                        let _ = swarm_core::settings::set(&settings, swarm_app::sound::SETTING, "");
                        self.set_status(StatusKind::Success, "messages make Swarm's own sound".to_string());
                        swarm_app::sound::play(swarm_app::sound::built_in());
                        return;
                    }
                    let path = expand_home(std::path::Path::new(&path));
                    match swarm_app::sound::from_file(&path) {
                        Ok(sound) => {
                            let _ = swarm_core::settings::set(&settings, swarm_app::sound::SETTING, &path.to_string_lossy());
                            self.set_status(StatusKind::Success, format!("messages make the sound of {}", path.display()));
                            swarm_app::sound::play(sound);
                        }
                        Err(e) => self.set_status(StatusKind::Error, format!("couldn't use that sound: {e:#}")),
                    }
                }
                KeyCode::Enter if prompt == prefs::Setting::Ringtone => {
                    self.name_prompt = None;
                    self.focus = Focus::Nav;
                    let path = std::mem::take(&mut self.input).trim().to_string();
                    let settings = paths().settings();
                    if path.is_empty() {
                        match swarm_app::ringtone::choose_built_in(&settings) {
                            Ok(()) => self.set_status(StatusKind::Success, "calls ring with Swarm's own ringtone".to_string()),
                            Err(e) => self.set_status(StatusKind::Error, format!("couldn't save that: {e:#}")),
                        }
                    } else {
                        match swarm_app::ringtone::choose(&settings, &expand_home(std::path::Path::new(&path))) {
                            Ok((_, name)) => self.set_status(StatusKind::Success, format!("calls ring with {name}")),
                            Err(e) => {
                                self.set_status(StatusKind::Error, format!("couldn't use that sound: {e:#}"));
                                return;
                            }
                        }
                    }
                    swarm_app::ringtone::play_once(&settings);
                }
                KeyCode::Enter if prompt == prefs::Setting::Unblock => {
                    self.name_prompt = None;
                    self.focus = Focus::Nav;
                    let typed = std::mem::take(&mut self.input);
                    let typed = typed.trim();
                    match actions::blocked(&self.node).into_iter().find(|r| r.label() == typed || r.key() == typed || (typed.len() >= 4 && r.key().starts_with(typed))) {
                        Some(row) => self.say(actions::unblock(&row)),
                        None => self.set_status(StatusKind::Error, format!("nobody blocked is called {typed}")),
                    }
                    self.refresh_rows();
                }
                KeyCode::Enter if prompt == prefs::Setting::ManualAddress => {
                    self.name_prompt = None;
                    self.focus = Focus::Nav;
                    let typed = std::mem::take(&mut self.input);
                    self.say(actions::set_manual_address(&self.node, &typed));
                }
                KeyCode::Enter if prompt == prefs::Setting::Status => {
                    self.name_prompt = None;
                    self.focus = Focus::Nav;
                    let says = std::mem::take(&mut self.input);
                    self.say(actions::set_status(&self.node, &says));
                }
                KeyCode::Enter => {
                    self.name_prompt = None;
                    self.focus = Focus::Nav;
                    let name = std::mem::take(&mut self.input);
                    let note = actions::set_name(&self.node, &name);
                    self.note(note);
                }
                _ => {
                    lineedit::edit(&mut self.input, &mut self.input_back, key);
                }
            }
            return;
        }
        if self.peer_form.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.peer_form = None;
                    self.focus = Focus::Nav;
                    self.input.clear();
                    self.set_status(StatusKind::Info, "cancelled");
                }
                KeyCode::Enter => self.advance_peer_form(),
                KeyCode::Tab | KeyCode::BackTab
                    if self.peer_form.as_ref().is_some_and(|f| matches!(f.kind, PeerFormKind::Add | PeerFormKind::Follow) && f.step == 1) =>
                {
                    let found: Vec<_> = swarm_app::discovery::nearby(&self.node).into_iter().filter(|n| !n.known).collect();
                    if found.is_empty() {
                        self.nearby_pick = None;
                        self.set_status(StatusKind::Info, "nobody new on this network is discoverable just now");
                    } else {
                        let count = found.len();
                        let next = match (self.nearby_pick, key.code) {
                            (None, KeyCode::BackTab) => count - 1,
                            (None, _) => 0,
                            (Some(i), KeyCode::BackTab) => (i + count - 1) % count,
                            (Some(i), _) => (i + 1) % count,
                        };
                        self.nearby_pick = Some(next);
                        self.input = found[next].code();
                    }
                }
                _ => {
                    lineedit::edit(&mut self.input, &mut self.input_back, key);
                }
            }
            return;
        }
        if self.channel_form.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.channel_form = None;
                    self.focus = Focus::Nav;
                    self.input.clear();
                    self.set_status(StatusKind::Info, "cancelled");
                }
                KeyCode::Enter => self.advance_channel_form(),
                _ => {
                    lineedit::edit(&mut self.input, &mut self.input_back, key);
                }
            }
            return;
        }
        if let Some(row) = self.receive_prompt.clone() {
            match key.code {
                KeyCode::Esc => {
                    self.receive_prompt = None;
                    self.focus = Focus::Nav;
                    self.input.clear();
                    self.set_status(StatusKind::Info, "cancelled");
                }
                KeyCode::Enter => {
                    self.receive_prompt = None;
                    self.focus = Focus::Nav;
                    let folder = std::mem::take(&mut self.input);
                    self.say(actions::set_receive(&row, &folder));
                }
                _ => {
                    lineedit::edit(&mut self.input, &mut self.input_back, key);
                }
            }
            return;
        }
        if self.post_compose.is_some() {
            self.handle_post_compose_key(key);
            return;
        }

        let in_chat = self.peer_tab == PeerTab::Chat && self.selected_peer().is_some();
        match key.code {
            KeyCode::Up if in_chat => self.pick(-1),
            KeyCode::Down if self.picked.is_some() => self.pick(1),
            KeyCode::Delete if self.picked.is_some() => self.delete_picked(),
            KeyCode::Esc if self.picked.is_some() => self.picked = None,
            KeyCode::Esc => {
                self.focus = Focus::Nav;
                self.input.clear();
            }
            KeyCode::Enter if key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) => {
                self.picked = None;
                lineedit::insert(&mut self.input, &mut self.input_back, "\n");
            }
            KeyCode::Enter => {
                self.picked = None;
                self.send_current_input().await;
            }
            _ => {
                self.picked = None;
                lineedit::edit(&mut self.input, &mut self.input_back, key);
            }
        }
    }

    /// Text pasted (the terminal marks it as pasted, so its line breaks
    /// don't arrive as Enter): into what's being typed, its line breaks
    /// kept in a message or post, made spaces anywhere else.
    pub(super) fn handle_paste(&mut self, pasted: &str) {
        let pasted = pasted.trim_end_matches(['\r', '\n']);
        if let Some((typed, _)) = self.locked.as_mut() {
            typed.push_str(&swarm_core::text::single_line(pasted));
            return;
        }
        if self.focus != Focus::Input || matches!(self.post_compose, Some(PostCompose::AskVisibility(_))) {
            return;
        }
        let lines_kept = self.typing_a_message() || self.post_compose.is_some();
        let text = if lines_kept { swarm_core::text::message_text(pasted) } else { swarm_core::text::single_line(pasted) };
        self.picked = None;
        self.picked_post = None;
        lineedit::insert(&mut self.input, &mut self.input_back, &text);
    }

    /// Whether what's typed is a message, to a contact or in a channel
    /// (not a form, a prompt or a post).
    fn typing_a_message(&self) -> bool {
        self.focus == Focus::Input
            && self.mode == MainMode::View
            && self.device_input.is_none()
            && self.name_prompt.is_none()
            && self.peer_form.is_none()
            && self.channel_form.is_none()
            && self.receive_prompt.is_none()
            && self.post_compose.is_none()
            && (self.selected_peer().is_some() || self.selected_channel().is_some())
    }

    /// Completes the name before the cursor to one in this chat (see
    /// `chat::complete`). Whether one fitted.
    fn complete_name(&mut self) -> bool {
        let names = if let Some(row) = self.selected_peer() {
            chat::peer_names(row)
        } else if let Some(id) = self.selected_channel().cloned() {
            chat::channel_names(&chat::channel_view(&self.node, &self.sidebar, &id, &self.prefs))
        } else {
            return false;
        };
        let cut = self.input.char_indices().nth(self.input.chars().count().saturating_sub(self.input_back)).map_or(self.input.len(), |(i, _)| i);
        let (before, after) = self.input.split_at(cut);
        match chat::complete(before, &names, &mut self.completing) {
            Some(done) => {
                self.input = format!("{done}{after}");
                true
            }
            None => false,
        }
    }

    pub(super) fn handle_post_compose_key(&mut self, key: KeyEvent) {
        let compose = self.post_compose.take().expect("guarded by post_compose.is_some() in the caller");
        match compose {
            PostCompose::Text if matches!(key.code, KeyCode::Up) || (self.picked_post.is_some() && matches!(key.code, KeyCode::Down | KeyCode::Delete | KeyCode::Esc)) => {
                match key.code {
                    KeyCode::Up => self.pick_post(-1),
                    KeyCode::Down => self.pick_post(1),
                    KeyCode::Delete => self.delete_picked_post(),
                    _ => self.picked_post = None,
                }
                self.post_compose = Some(PostCompose::Text);
            }
            PostCompose::Text => match key.code {
                KeyCode::Esc => {
                    self.focus = Focus::Nav;
                    self.input.clear();
                    self.set_status(StatusKind::Info, "cancelled");
                }
                KeyCode::Enter if key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) => {
                    self.picked_post = None;
                    lineedit::insert(&mut self.input, &mut self.input_back, "\n");
                    self.post_compose = Some(PostCompose::Text);
                }
                KeyCode::Enter => {
                    self.picked_post = None;
                    let text = std::mem::take(&mut self.input);
                    if text.trim().is_empty() {
                        self.focus = Focus::Nav;
                        self.set_status(StatusKind::Info, "empty post, not saved");
                    } else {
                        self.post_compose = Some(PostCompose::AskVisibility(text));
                        self.set_status(StatusKind::Info, "");
                    }
                }
                _ => {
                    self.picked_post = None;
                    lineedit::edit(&mut self.input, &mut self.input_back, key);
                    self.post_compose = Some(PostCompose::Text);
                }
            },
            PostCompose::AskVisibility(text) => match key.code {
                KeyCode::Esc => {
                    self.focus = Focus::Nav;
                    self.set_status(StatusKind::Info, "cancelled");
                }
                KeyCode::Char('y') | KeyCode::Char('Y') => self.submit_post(text, true),
                KeyCode::Char('n') | KeyCode::Char('N') => self.submit_post(text, false),
                _ => self.post_compose = Some(PostCompose::AskVisibility(text)),
            },
        }
    }
}

/// A contact line typed into the peer form's address or key: its name
/// (unless one's typed), addresses and key taken, and on to the cover
/// rate (following, which has none, done). False if `value` isn't one.
pub(super) fn from_contact_line(form: &mut PeerForm, value: &str) -> bool {
    let Some(contact) = swarm_app::contact::parse(value) else { return false };
    if !matches!(form.step, 1 | 2) {
        return false;
    }
    if form.name.is_empty() && form.kind != PeerFormKind::Follow {
        form.name = contact.name.clone();
    }
    form.address = contact.address_text();
    form.pubkey = contact.key_text();
    form.step = if form.kind == PeerFormKind::Follow { 2 } else { 3 };
    true
}
