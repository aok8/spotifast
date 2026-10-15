//! An isolated playlist rotation workflow, using the client's existing controls.

use egui::{Id, RichText};

use crate::api::models::Playlist;
use crate::app::App;
use crate::model::Action;
use crate::rotation::{Request, Stage};
use crate::theme::{self, Icon};

#[derive(Clone, Default)]
struct Choices {
    source: Option<String>,
    archive: Option<String>,
    source_filter: String,
    archive_filter: String,
    saved: (Option<String>, Option<String>),
}

#[derive(Clone)]
struct Removal {
    key: String,
    name: String,
    archive: Option<String>,
    archive_id: Option<String>,
    without_archive: bool,
    require_active: bool,
    device_id: Option<String>,
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    ui.add_space(8.0);
    theme::text(ui, "Rotation", theme::bold(28.0), palette.text);
    paragraph(
        ui,
        "Listen through a shuffled playlist. Keep the songs you want, remove the rest.",
        theme::regular(14.0),
        palette.secondary,
    );
    ui.add_space(20.0);
    configuration(app, ui);
    ui.add_space(18.0);
    output(app, ui);
    ui.add_space(12.0);
    if app.rotation_active {
        if theme::pill_button(ui, &palette, "Turn off", false).clicked() {
            app.actions.push(Action::RotationTurnOff);
        }
    } else {
        ui.add_enabled_ui(
            !app.rotation_busy
                && !app.rotation_playback_pending
                && !app.rotation.unresolved()
                && app.rotation.current().is_some()
                && app.rotation_device.is_some(),
            |ui| {
                if theme::pill_button(ui, &palette, "Turn on", true).clicked() {
                    app.actions.push(Action::RotationTurnOn);
                }
            },
        );
    }
    if app.rotation_busy {
        ui.horizontal(|ui| {
            theme::spinner(ui, 16.0, palette.accent);
            theme::text(
                ui,
                "Updating Rotation…",
                theme::regular(14.0),
                palette.secondary,
            );
        });
    }
    if app.rotation_playback_pending {
        paragraph(
            ui,
            "Waiting for the selected device to confirm playback…",
            theme::regular(14.0),
            palette.secondary,
        );
    }
    if let Some(error) = &app.rotation_error {
        ui.add_space(12.0);
        ui.add(egui::Label::new(RichText::new(error).color(palette.danger)).wrap());
    }
    recovery(app, ui);
    ui.add_space(20.0);
    let index = app.rotation.current_index;
    let total = app.rotation.order.len();
    let upcoming: Vec<_> = app
        .rotation
        .ordered_items()
        .into_iter()
        .skip(index + 1)
        .take(20)
        .cloned()
        .collect();
    if let Some(current) = app.rotation.current().cloned() {
        theme::text(ui, "Current song", theme::bold(18.0), palette.text);
        ui.add_space(8.0);
        song(ui, &palette, &current.track, true);
        paragraph(
            ui,
            if app.rotation_active {
                "Rotation is on. Use the player’s X to remove and skip. Previous and Next keep songs in your playlists."
            } else {
                "Turn on Rotation to play this order and edit with the player’s X button."
            },
            theme::regular(12.0),
            palette.secondary,
        );
        ui.add_space(22.0);
        theme::text(
            ui,
            format!("Upcoming · {} songs", total.saturating_sub(index + 1)),
            theme::bold(18.0),
            palette.text,
        );
        ui.add_space(8.0);
        for item in &upcoming {
            song(ui, &palette, &item.track, false);
        }
    } else if app.rotation.source.is_none() {
        theme::text(
            ui,
            "Choose a source playlist to begin.",
            theme::regular(16.0),
            palette.secondary,
        );
    } else if !app.rotation_busy {
        theme::text(
            ui,
            if app.rotation.order.is_empty() {
                "No songs in this rotation"
            } else {
                "Rotation complete"
            },
            theme::bold(18.0),
            palette.text,
        );
        paragraph(
            ui,
            "Start a new rotation to load the source playlist and shuffle its playable songs.",
            theme::regular(14.0),
            palette.secondary,
        );
    }
}

fn paragraph(ui: &mut egui::Ui, text: impl Into<String>, font: egui::FontId, color: egui::Color32) {
    ui.add(egui::Label::new(RichText::new(text.into()).font(font).color(color)).wrap());
}

fn configuration(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let account = app.user_id().unwrap_or_default().to_owned();
    let id = Id::new(("rotation-choices", account));
    let mut choices = ui
        .ctx()
        .data(|data| data.get_temp::<Choices>(id))
        .unwrap_or_else(|| Choices {
            source: app.rotation.source.as_ref().map(|p| p.id.clone()),
            archive: app.rotation.archive.as_ref().map(|p| p.id.clone()),
            ..Choices::default()
        });
    let saved = (
        app.rotation.source.as_ref().map(|p| p.id.clone()),
        app.rotation.archive.as_ref().map(|p| p.id.clone()),
    );
    if choices.saved != saved {
        choices.source.clone_from(&saved.0);
        choices.archive.clone_from(&saved.1);
        choices.saved = saved;
    }
    let mut playlists: Vec<_> = app
        .library
        .playlists
        .get()
        .into_iter()
        .flatten()
        .filter(|p| app.can_edit_playlist(p))
        .cloned()
        .collect();
    for configured in [app.rotation.source.as_ref(), app.rotation.archive.as_ref()]
        .into_iter()
        .flatten()
    {
        if !playlists.iter().any(|p| p.id == configured.id) {
            playlists.push(configured.clone());
        }
    }
    theme::text(ui, "Source playlist", theme::bold(14.0), palette.text);
    picker(
        ui,
        "rotation-source",
        &playlists,
        &mut choices.source,
        &mut choices.source_filter,
        None,
        false,
    );
    ui.add_space(10.0);
    theme::text(ui, "History playlist", theme::bold(14.0), palette.text);
    picker(
        ui,
        "rotation-archive",
        &playlists,
        &mut choices.archive,
        &mut choices.archive_filter,
        choices.source.as_deref(),
        true,
    );
    if choices.archive.is_some() && choices.archive == choices.source {
        choices.archive = None;
    }
    paragraph(
        ui,
        if choices.archive.is_some() {
            "Removed songs are added to history before leaving the source."
        } else {
            "None: removed songs leave the source without being archived."
        },
        theme::regular(12.0),
        palette.secondary,
    );
    let changed = choices.source.as_deref() != app.rotation.source.as_ref().map(|p| p.id.as_str())
        || choices.archive.as_deref() != app.rotation.archive.as_ref().map(|p| p.id.as_str());
    ui.add_space(12.0);
    ui.horizontal_wrapped(|ui| {
        ui.add_enabled_ui(!app.rotation_busy && !app.rotation.unresolved(), |ui| {
            ui.add_enabled_ui(changed && choices.source.is_some(), |ui| {
                if theme::pill_button(ui, &palette, "Use playlists", true).clicked()
                    && let Some(source) = playlists
                        .iter()
                        .find(|p| Some(&p.id) == choices.source.as_ref())
                {
                    let archive = playlists
                        .iter()
                        .find(|p| Some(&p.id) == choices.archive.as_ref())
                        .cloned();
                    app.actions.push(Action::Rotation(Request::Configure {
                        source: Box::new(source.clone()),
                        archive: archive.map(Box::new),
                    }));
                }
            });
            ui.add_enabled_ui(app.rotation.source.is_some() && !changed, |ui| {
                let label = if app.rotation.order.is_empty() {
                    "Start rotation"
                } else {
                    "Restart rotation"
                };
                if theme::pill_button(ui, &palette, label, false).clicked() {
                    app.actions.push(Action::Rotation(Request::Restart));
                }
            });
        });
    });
    if changed && !app.rotation.order.is_empty() {
        paragraph(
            ui,
            "Changing playlists clears the saved rotation.",
            theme::regular(12.0),
            palette.secondary,
        );
    }
    ui.ctx().data_mut(|data| data.insert_temp(id, choices));
}

fn picker(
    ui: &mut egui::Ui,
    id: &str,
    playlists: &[Playlist],
    selected: &mut Option<String>,
    filter: &mut String,
    excluded: Option<&str>,
    allow_none: bool,
) {
    let label = selected
        .as_ref()
        .and_then(|id| playlists.iter().find(|p| &p.id == id))
        .map(|p| p.name.as_str())
        .unwrap_or(if allow_none {
            "None"
        } else {
            "Choose a playlist"
        });
    egui::ComboBox::from_id_salt(id)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .width(ui.available_width().min(420.0))
        .selected_text(label)
        .show_ui(ui, |ui| {
            ui.add(egui::TextEdit::singleline(filter).hint_text("Find a playlist"));
            if allow_none && ui.selectable_value(selected, None, "None").clicked() {
                ui.close();
            }
            let filter = filter.to_lowercase();
            for playlist in playlists.iter().filter(|p| {
                Some(p.id.as_str()) != excluded && p.name.to_lowercase().contains(&filter)
            }) {
                if ui
                    .selectable_value(selected, Some(playlist.id.clone()), &playlist.name)
                    .clicked()
                {
                    ui.close();
                }
            }
            if playlists.is_empty() {
                ui.label("No editable playlists loaded.");
            }
        });
}

fn output(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    theme::text(ui, "Play on", theme::bold(14.0), palette.text);
    ui.horizontal_wrapped(|ui| {
        let selected = app
            .devices
            .iter()
            .find(|d| d.id == app.rotation_device && d.id.is_some())
            .map(|d| d.name.as_str())
            .unwrap_or("Choose a device");
        egui::ComboBox::from_id_salt("rotation-device")
            .width(ui.available_width().min(330.0))
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for device in &app.devices {
                    ui.add_enabled_ui(!device.is_restricted && device.id.is_some(), |ui| {
                        if ui
                            .selectable_label(device.id == app.rotation_device, &device.name)
                            .clicked()
                            && let Some(id) = &device.id
                        {
                            app.actions.push(Action::RotationSelectDevice(id.clone()));
                        }
                    });
                }
            });
        if theme::icon_button(
            ui,
            Icon::Refresh,
            18.0,
            palette.secondary,
            palette.text,
            "Refresh devices",
        )
        .clicked()
        {
            app.actions.push(Action::RefreshDevices);
        }
    });
    if app.rotation_device.is_none() {
        paragraph(
            ui,
            "Choose an unrestricted Spotify Connect device to play. You can still organize your rotation.",
            theme::regular(12.0),
            palette.secondary,
        );
    }
}

fn song(
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    track: &crate::api::models::Track,
    current: bool,
) {
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                let size = if current { 64.0 } else { 40.0 };
                let image = track
                    .album
                    .as_ref()
                    .and_then(|album| crate::api::models::pick_image(&album.images, size as u32));
                super::widgets::cover(ui, palette, image, size, 4.0, Icon::Music);
                ui.vertical(|ui| {
                    theme::text(
                        ui,
                        &track.name,
                        if current {
                            theme::bold(20.0)
                        } else {
                            theme::medium(14.0)
                        },
                        palette.text,
                    );
                    let artists = track
                        .artists
                        .iter()
                        .map(|a| a.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    theme::text(ui, artists, theme::regular(13.0), palette.secondary);
                });
            });
        });
    ui.separator();
}

fn recovery(app: &mut App, ui: &mut egui::Ui) {
    let Some(operation) = app.rotation.operation.as_ref() else {
        return;
    };
    let stage = operation.stage;
    let palette = app.palette;
    let message = match stage {
        Stage::Pending => {
            "An interrupted edit needs your review. Check the source and history playlists in Spotify before continuing."
        }
        Stage::ArchiveUncertain => {
            "History may already contain this song. Check the history playlist before continuing. Rotation will not add it again."
        }
        Stage::Archived => {
            "The song is in history, but is still in the source. Retry removes it from the source only."
        }
        Stage::DeleteUncertain => {
            "Source removal could not be confirmed. Check the source playlist, then reload Rotation. Do not repeat removal blindly."
        }
        _ => return,
    };
    ui.add_space(12.0);
    ui.add(egui::Label::new(RichText::new(message).color(palette.warning)).wrap());
    ui.add_enabled_ui(!app.rotation_busy, |ui| {
        if stage == Stage::Archived
            && theme::pill_button(ui, &palette, "Retry source removal", false).clicked()
        {
            app.actions.push(Action::Rotation(Request::Retry));
        }
        if matches!(stage, Stage::Pending | Stage::ArchiveUncertain)
            && theme::pill_button(ui, &palette, "Remove without another archive add", false)
                .clicked()
            && let Some(current) = app.rotation.current()
        {
            ui.ctx().data_mut(|data| {
                data.insert_temp(
                    Id::new("rotation-removal"),
                    Removal {
                        key: current.key.clone(),
                        name: current.track.name.clone(),
                        archive: None,
                        archive_id: app.rotation.archive.as_ref().map(|p| p.id.clone()),
                        without_archive: true,
                        require_active: false,
                        device_id: None,
                    },
                )
            });
        }
        if theme::pill_button(ui, &palette, "Reload after checking Spotify", false).clicked() {
            ui.ctx()
                .data_mut(|data| data.insert_temp(Id::new("rotation-resolve"), true));
        }
    });
    let id = Id::new("rotation-resolve");
    if ui
        .ctx()
        .data(|data| data.get_temp::<bool>(id))
        .unwrap_or(false)
    {
        let mut close = false;
        let response = egui::Modal::new(id).show(ui.ctx(), |ui| {
            ui.set_max_width(400.0);
            theme::text(ui, "Reload Rotation?", theme::bold(20.0), palette.text);
            ui.add(egui::Label::new("Check the source and history playlists in Spotify first. Reloading clears this recovery record and builds a new shuffled order. It will not repeat any playlist edit.").wrap());
            ui.add_space(14.0);
            if theme::pill_button(ui, &palette, "I checked Spotify. Reload Rotation", true).clicked() {
                app.actions.push(Action::Rotation(Request::ResolveReload));
                close = true;
            }
            if theme::pill_button(ui, &palette, "Cancel", false).clicked() { close = true; }
        });
        if close || response.should_close() {
            ui.ctx().data_mut(|data| data.remove::<bool>(id));
        }
    }
}

/// Snapshot the occurrence and archive before showing a destructive edit.
/// The footer can open this on any page; drawing still only emits actions.
pub(crate) fn request_removal(app: &App, ctx: &egui::Context) {
    if !app.rotation_can_remove() {
        return;
    }
    let Some(current) = app.rotation.current() else {
        return;
    };
    ctx.data_mut(|data| {
        data.insert_temp(
            Id::new("rotation-removal"),
            Removal {
                key: current.key.clone(),
                name: current.track.name.clone(),
                archive: app
                    .rotation
                    .archive
                    .as_ref()
                    .map(|playlist| playlist.name.clone()),
                archive_id: app
                    .rotation
                    .archive
                    .as_ref()
                    .map(|playlist| playlist.id.clone()),
                without_archive: false,
                require_active: true,
                device_id: app.rotation_device.clone(),
            },
        )
    });
}

pub(crate) fn confirmation(app: &mut App, ctx: &egui::Context) {
    let id = Id::new("rotation-removal");
    let Some(removal) = ctx.data(|data| data.get_temp::<Removal>(id)) else {
        return;
    };
    if app
        .rotation
        .current()
        .is_none_or(|item| item.key != removal.key)
        || app.rotation.archive.as_ref().map(|p| &p.id) != removal.archive_id.as_ref()
        || (removal.require_active
            && (!app.rotation_active || app.rotation_device != removal.device_id))
    {
        ctx.data_mut(|data| data.remove::<Removal>(id));
        return;
    }
    let palette = app.palette;
    let mut close = false;
    let response = egui::Modal::new(id).show(ctx, |ui| {
        ui.set_max_width(400.0);
        let active = app.rotation_active;
        theme::text(
            ui,
            if active {
                "Remove and skip?"
            } else {
                "Remove song?"
            },
            theme::bold(20.0),
            palette.text,
        );
        ui.add_space(10.0);
        let playback = if active {
            "The next song starts only after removal succeeds."
        } else {
            "Your current playback continues."
        };
        let text = if let Some(archive) = &removal.archive {
            format!(
                "Add “{}” to “{}”, then remove it from the source playlist. {playback}",
                removal.name, archive
            )
        } else {
            format!(
                "Remove “{}” from the source playlist without adding it to history. {playback}",
                removal.name
            )
        };
        ui.add(egui::Label::new(text).wrap());
        ui.add_space(14.0);
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!removal.require_active || app.rotation_can_remove(), |ui| {
                if theme::pill_button(
                    ui,
                    &palette,
                    if active {
                        "Remove and skip"
                    } else {
                        "Remove from source"
                    },
                    true,
                )
                .clicked()
                {
                    app.actions.push(Action::Rotation(Request::Remove {
                        item_key: removal.key.clone(),
                        without_archive: removal.without_archive,
                    }));
                    close = true;
                }
            });
            if theme::pill_button(ui, &palette, "Cancel", false).clicked() {
                close = true;
            }
        });
    });
    if close || response.should_close() {
        ctx.data_mut(|data| data.remove::<Removal>(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rotation::{Entry, Operation, State};

    fn app() -> App {
        let root = std::env::current_dir()
            .unwrap()
            .join(".cache/rotation-ui-tests");
        let mut app = App::new(
            &crate::backend::Waker::default(),
            crate::paths::AppDirs {
                config: root.join("config"),
                state: root.join("state"),
                cache: root.join("cache"),
            },
            crate::settings::Settings::default(),
            crate::app::AppOptions {
                media_controls: false,
                restore_sign_in: false,
                tray: false,
            },
        );
        crate::demo::populate(&mut app);
        app.rotation = State::new(app.user_id().unwrap().into());
        app.rotation.source = Some(Playlist {
            id: "source".into(),
            name: "Source".into(),
            ..Default::default()
        });
        app.rotation.items = vec![Entry {
            key: "0:spotify:track:song".into(),
            raw_position: 0,
            track: crate::api::models::Track {
                name: "Current song".into(),
                uri: "spotify:track:song".into(),
                ..Default::default()
            },
        }];
        app.rotation.order = vec![app.rotation.items[0].key.clone()];
        app.actions.clear();
        app
    }

    #[test]
    fn playlist_picker_keeps_filter_open_until_a_choice_is_clicked() {
        for allow_none in [false, true] {
            let ctx = egui::Context::default();
            ctx.enable_accesskit();
            theme::install(&ctx);
            let playlists = vec![
                Playlist {
                    id: "alpha".into(),
                    name: "Alpha".into(),
                    ..Default::default()
                },
                Playlist {
                    id: "beta".into(),
                    name: "Beta".into(),
                    ..Default::default()
                },
            ];
            let mut selected = None;
            let mut filter = String::new();
            {
                let mut frame = |events| {
                    let mut output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(800.0, 600.0),
                            )),
                            events,
                            ..Default::default()
                        },
                        |ui| {
                            egui::CentralPanel::default().show(ui, |ui| {
                                picker(
                                    ui,
                                    "picker-test",
                                    &playlists,
                                    &mut selected,
                                    &mut filter,
                                    None,
                                    allow_none,
                                );
                            });
                        },
                    );
                    output.textures_delta.clear();
                    output.platform_output.accesskit_update.unwrap()
                };
                let center = |tree: &egui::accesskit::TreeUpdate, label: &str| {
                    let bounds = tree
                        .nodes
                        .iter()
                        .find(|(_, node)| {
                            (node.label() == Some(label)
                                || node.value() == Some(label)
                                || (label == "Find a playlist"
                                    && node.role() == egui::accesskit::Role::TextInput))
                                && node.bounds().is_some()
                        })
                        .unwrap_or_else(|| {
                            panic!(
                                "missing {label}: {:?}",
                                tree.nodes
                                    .iter()
                                    .map(|(_, node)| (node.role(), node.label(), node.value()))
                                    .collect::<Vec<_>>()
                            )
                        })
                        .1
                        .bounds()
                        .unwrap();
                    egui::pos2(
                        ((bounds.x0 + bounds.x1) / 2.0) as f32,
                        ((bounds.y0 + bounds.y1) / 2.0) as f32,
                    )
                };
                let click = |pos| {
                    vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            modifiers: egui::Modifiers::NONE,
                        },
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed: false,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ]
                };
                frame(vec![]);
                let tree = frame(vec![]);
                frame(click(center(
                    &tree,
                    if allow_none {
                        "None"
                    } else {
                        "Choose a playlist"
                    },
                )));
                let tree = frame(vec![]);
                frame(click(center(&tree, "Find a playlist")));
                assert!(
                    egui::Popup::is_any_open(&ctx),
                    "clicking the filter must keep the picker open"
                );
                let tree = frame(vec![egui::Event::Text("Beta".into())]);
                assert!(
                    !tree
                        .nodes
                        .iter()
                        .any(|(_, node)| node.label() == Some("Alpha")),
                    "typing filters the choices"
                );
                let target = center(&tree, "Beta");
                frame(click(target));
                assert!(
                    !egui::Popup::is_any_open(&ctx),
                    "choosing a playlist closes the picker"
                );
                let tree = frame(vec![]);
                frame(click(center(&tree, "Beta")));
                let tree = frame(vec![]);
                if allow_none {
                    frame(click(center(&tree, "None")));
                    assert!(
                        !egui::Popup::is_any_open(&ctx),
                        "choosing None closes the history picker"
                    );
                } else {
                    frame(click(egui::pos2(700.0, 500.0)));
                    assert!(
                        !egui::Popup::is_any_open(&ctx),
                        "clicking outside closes the picker"
                    );
                }
            }
            assert_eq!(filter, "Beta");
            assert_eq!(selected.as_deref(), (!allow_none).then_some("beta"));
        }
    }

    #[test]
    fn rotation_renders_all_recovery_stages_without_editing_state() {
        let mut app = app();
        for dark in [false, true] {
            app.palette = if dark {
                theme::Palette::dark()
            } else {
                theme::Palette::light()
            };
            for width in [360.0, 900.0] {
                for stage in [
                    Stage::ArchiveUncertain,
                    Stage::Archived,
                    Stage::DeleteUncertain,
                ] {
                    app.rotation.operation = Some(Operation {
                        item_key: app.rotation.order[0].clone(),
                        stage,
                    });
                    let before = app.rotation.clone();
                    let ctx = egui::Context::default();
                    theme::install(&ctx);
                    let mut output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(width, 900.0),
                            )),
                            ..Default::default()
                        },
                        |ui| {
                            egui::CentralPanel::default().show(ui, |ui| show(&mut app, ui));
                        },
                    );
                    output.textures_delta.clear();
                    assert_eq!(app.rotation, before);
                    assert!(
                        app.actions.is_empty(),
                        "Drawing must not send playlist or playback commands"
                    );
                }
            }
        }
        app.backend.shutdown();
    }

    #[test]
    fn rotation_has_a_mode_button_without_duplicate_transport() {
        let mut app = app();
        for active in [false, true] {
            app.rotation_active = active;
            let ctx = egui::Context::default();
            theme::install(&ctx);
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    egui::CentralPanel::default().show(ui, |ui| show(&mut app, ui));
                },
            );
            output.textures_delta.clear();
            let labels: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape {
                        Some(text.galley.text())
                    } else {
                        None
                    }
                })
                .collect();
            assert!(labels.contains(&if active { "Turn off" } else { "Turn on" }));
            for duplicate in [
                "Previous",
                "Next",
                "Play",
                "Pause",
                "Resume",
                "Remove and skip",
            ] {
                assert!(
                    !labels.contains(&duplicate),
                    "duplicate Rotation transport: {duplicate}"
                );
            }
            assert!(app.actions.is_empty());
        }
        app.backend.shutdown();
    }

    #[test]
    fn footer_confirmation_closes_when_rotation_mode_or_target_changes() {
        use crate::api::models::{Device, PlayableItem, PlaybackState};
        let mut app = app();
        app.rotation_active = true;
        app.rotation_device = Some("rotation-device".into());
        app.rotation.playback = Some(PlaybackState {
            device: Some(Device {
                id: app.rotation_device.clone(),
                is_active: true,
                ..Default::default()
            }),
            repeat_state: "off".into(),
            item: Some(PlayableItem::Track(
                app.rotation.current().unwrap().track.clone(),
            )),
            ..Default::default()
        });
        let ctx = egui::Context::default();
        let id = Id::new("rotation-removal");
        request_removal(&app, &ctx);
        assert!(ctx.data(|data| data.get_temp::<Removal>(id)).is_some());
        app.rotation_active = false;
        confirmation(&mut app, &ctx);
        assert!(ctx.data(|data| data.get_temp::<Removal>(id)).is_none());
        assert!(app.actions.is_empty());
        app.rotation_active = true;
        request_removal(&app, &ctx);
        assert!(ctx.data(|data| data.get_temp::<Removal>(id)).is_some());
        app.rotation_device = Some("different-device".into());
        confirmation(&mut app, &ctx);
        assert!(ctx.data(|data| data.get_temp::<Removal>(id)).is_none());
        assert!(app.actions.is_empty());
        app.backend.shutdown();
    }

    #[test]
    fn stale_removal_confirmation_cannot_target_a_changed_archive() {
        let mut app = app();
        app.rotation.archive = Some(Playlist {
            id: "new-archive".into(),
            ..Default::default()
        });
        let ctx = egui::Context::default();
        let id = Id::new("rotation-removal");
        ctx.data_mut(|data| {
            data.insert_temp(
                id,
                Removal {
                    key: app.rotation.order[0].clone(),
                    name: "Song".into(),
                    archive: Some("Old archive".into()),
                    archive_id: Some("old-archive".into()),
                    without_archive: false,
                    require_active: false,
                    device_id: None,
                },
            )
        });
        confirmation(&mut app, &ctx);
        assert!(ctx.data(|data| data.get_temp::<Removal>(id)).is_none());
        assert!(app.actions.is_empty());
        app.backend.shutdown();
    }
}
