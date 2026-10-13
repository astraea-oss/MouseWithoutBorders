//! Pairing a phone: a QR code from the running receiver, then the result.

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use eframe::egui::{self, Align, Color32, Layout, Rect, RichText, Sense, Ui, Vec2};
use qrcode::QrCode;

use crate::{
    NodeLink, PhonePairingEvent,
    widgets::{FAINT, MUTED, OK, primary_button},
};

const QR_SIZE: f32 = 220.0;

enum View {
    Starting,
    Showing {
        uri: String,
        code: QrCode,
        deadline: Instant,
    },
    Paired(String),
    Expired,
    Failed(String),
}

/// What the user did with the pairing panel this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingAction {
    None,
    Close,
}

pub struct PhonePairing {
    events: Option<mpsc::Receiver<PhonePairingEvent>>,
    view: View,
}

impl PhonePairing {
    /// Asks the receiver to open a pairing window.
    pub fn start(link: &dyn NodeLink) -> Self {
        match link.pair_phone() {
            Ok(events) => Self {
                events: Some(events),
                view: View::Starting,
            },
            Err(error) => Self {
                events: None,
                view: View::Failed(format!("{error:#}")),
            },
        }
    }

    pub fn paired(&self) -> bool {
        matches!(self.view, View::Paired(_))
    }

    fn poll(&mut self) {
        if let Some(events) = &self.events {
            loop {
                match events.try_recv() {
                    Ok(PhonePairingEvent::Offer { uri, expires_in }) => {
                        self.view = match QrCode::new(uri.as_bytes()) {
                            Ok(code) => View::Showing {
                                uri,
                                code,
                                deadline: Instant::now() + expires_in,
                            },
                            Err(_) => {
                                View::Failed("the pairing link is too long for a QR code".into())
                            }
                        };
                    }
                    Ok(PhonePairingEvent::Paired(name)) => self.view = View::Paired(name),
                    Ok(PhonePairingEvent::Closed) => {
                        if !matches!(self.view, View::Paired(_)) {
                            self.view = View::Expired;
                        }
                    }
                    Ok(PhonePairingEvent::Failed(error)) => self.view = View::Failed(error),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.events = None;
                        if matches!(self.view, View::Starting | View::Showing { .. }) {
                            self.view = View::Failed("lost contact with edge-kvm".into());
                        }
                        break;
                    }
                }
            }
        }
        if let View::Showing { deadline, .. } = self.view
            && Instant::now() >= deadline
        {
            self.view = View::Expired;
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, link: &dyn NodeLink) -> PairingAction {
        self.poll();
        ui.ctx().request_repaint_after(Duration::from_millis(500));
        let mut action = PairingAction::None;
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            match &self.view {
                View::Paired(name) => {
                    ui.label(RichText::new(format!("{name} added")).strong().size(15.0));
                    ui.label(
                        RichText::new(
                            "Clipboard and notifications are on. Change them in Settings.",
                        )
                        .color(MUTED),
                    );
                    paint_check(ui);
                }
                _ => {
                    ui.label(RichText::new("Scan with HyprMobile").strong().size(15.0));
                    ui.label(
                        RichText::new("Open HyprMobile on the phone and scan this code")
                            .color(MUTED),
                    );
                    match &self.view {
                        View::Showing { code, .. } => paint_qr(ui, code),
                        _ => {
                            ui.allocate_exact_size(Vec2::splat(QR_SIZE), Sense::hover());
                        }
                    }
                }
            }

            match &self.view {
                View::Starting => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.label(RichText::new("Opening pairing…").color(MUTED));
                    });
                }
                View::Showing { uri, deadline, .. } => {
                    let left = deadline.saturating_duration_since(Instant::now()).as_secs();
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.label(RichText::new("Waiting for the phone…").color(MUTED));
                        ui.label(
                            RichText::new(format!("{}:{:02}", left / 60, left % 60))
                                .monospace()
                                .color(FAINT),
                        );
                    });
                    egui::CollapsingHeader::new(
                        RichText::new("Can't scan? Show the link").color(FAINT),
                    )
                    .id_salt("pairing_link")
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(uri).monospace().small())
                                .wrap()
                                .selectable(true),
                        );
                        if ui.small_button("Copy link").clicked() {
                            ui.ctx().copy_text(uri.clone());
                        }
                    });
                }
                View::Expired => {
                    ui.label(RichText::new("The code expired.").color(MUTED));
                }
                View::Failed(error) => {
                    ui.label(RichText::new(format!("Couldn't open pairing: {error}")).color(MUTED));
                }
                View::Paired(_) => {}
            }
        });

        ui.add_space(6.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| match self.view {
            View::Paired(_) => {
                if primary_button(ui, "Done").clicked() {
                    action = PairingAction::Close;
                }
            }
            View::Expired | View::Failed(_) => {
                if primary_button(ui, "Show a new code").clicked() {
                    *self = Self::start(link);
                }
                if ui.button("Close").clicked() {
                    action = PairingAction::Close;
                }
            }
            View::Starting | View::Showing { .. } => {
                if ui.button("Cancel").clicked() {
                    action = PairingAction::Close;
                }
                ui.label(
                    RichText::new("The phone must be on the same network")
                        .small()
                        .color(FAINT),
                );
            }
        });
        action
    }
}

/// Dark modules on white, with a quiet zone, snapped to whole pixels.
fn paint_qr(ui: &mut Ui, code: &QrCode) {
    const QUIET: usize = 3;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(QR_SIZE), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 10.0, Color32::WHITE);
    let width = code.width();
    let modules = width + 2 * QUIET;
    let cell = (QR_SIZE / modules as f32).floor().max(1.0);
    let origin = rect.center() - Vec2::splat(cell * modules as f32 / 2.0);
    for (index, color) in code.to_colors().iter().enumerate() {
        if *color != qrcode::Color::Dark {
            continue;
        }
        let x = (index % width + QUIET) as f32;
        let y = (index / width + QUIET) as f32;
        painter.rect_filled(
            Rect::from_min_size(origin + Vec2::new(x * cell, y * cell), Vec2::splat(cell)),
            0.0,
            Color32::from_rgb(0x11, 0x11, 0x11),
        );
    }
}

fn paint_check(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(QR_SIZE), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 10.0, Color32::from_rgb(0x18, 0x39, 0x2c));
    let c = rect.center();
    let stroke = egui::Stroke::new(8.0, OK);
    painter.line_segment(
        [c + Vec2::new(-34.0, 2.0), c + Vec2::new(-10.0, 26.0)],
        stroke,
    );
    painter.line_segment(
        [c + Vec2::new(-10.0, 26.0), c + Vec2::new(38.0, -24.0)],
        stroke,
    );
}
