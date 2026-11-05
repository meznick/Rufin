//! iTunes source setup: pick the `iTunes Library.itl` and an optional
//! media-folder root override (the local root of the guest "Music Folder";
//! defaults to the parent directory of the `.itl`).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use super::{EditableSource, begin_connect_attempt, setup_scaffold, SetupActions, SetupViewContext, SourcePresentation, SourceSetupFlow};
use crate::Preferences;
use gtk_widgets::controls::text_button;
use localization::tr;

#[derive(Default)]
struct Draft {
    library: String,
    media_folder: String,
}

struct ItunesSetupFlow {
    presentation: &'static SourcePresentation,
    draft: Rc<RefCell<Draft>>,
}

pub(super) fn setup_flow(
    _shell: &Rc<Preferences>,
    presentation: &'static SourcePresentation,
) -> Rc<dyn SourceSetupFlow> {
    Rc::new(ItunesSetupFlow {
        presentation,
        draft: Rc::new(RefCell::new(Draft::default())),
    })
}

impl SourceSetupFlow for ItunesSetupFlow {
    fn view(&self, shell: &Rc<Preferences>, context: &SetupViewContext) -> gtk::Widget {
        let (scroller, content, actions, status) =
            setup_scaffold(shell, context, self.presentation);

        // The connect button flips with the library path: the scaffold only
        // re-evaluates readiness on surface changes, so keep it wired to the
        // entry text directly.
        let connect = text_button("rufin-network-server-symbolic", "Connect");
        connect.add_css_class("suggested-action");
        connect.set_sensitive(false);
        let draft_for_ready = Rc::clone(&self.draft);
        let update_ready: Rc<dyn Fn()> = Rc::new({
            let connect = connect.downgrade();
            move || {
                if let Some(connect) = connect.upgrade() {
                    connect.set_sensitive(!draft_for_ready.borrow().library.trim().is_empty());
                }
            }
        });

        let group = adw::PreferencesGroup::new();
        group.set_title(&tr("iTunes library"));
        group.set_description(Some(&tr(
            "The iTunes Library.itl file. Ratings and play counts sync back into it. Do not run iTunes on this library at the same time. The media folder root defaults to the folder containing the library file.",
        )));

        let library_row = adw::EntryRow::new();
        library_row.set_title(&tr("Library file"));
        library_row.set_text(&self.draft.borrow().library);
        library_row.set_input_purpose(gtk::InputPurpose::Url);
        {
            let draft = Rc::clone(&self.draft);
            let update_ready = Rc::clone(&update_ready);
            library_row.connect_changed(move |row| {
                draft.borrow_mut().library = row.text().to_string();
                update_ready();
            });
        }
        let library_browse = text_button("folder-open-symbolic", "Browse…");
        library_row.add_suffix(&library_browse);
        {
            let window = shell.window.clone();
            let row = library_row.clone();
            library_browse.connect_clicked(move |_| {
                let dialog = gtk::FileDialog::builder()
                    .title(tr("Choose the iTunes Library"))
                    .build();
                let filter = gtk::FileFilter::new();
                filter.set_name(Some("iTunes Library (.itl)"));
                filter.add_pattern("*.itl");
                let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);
                dialog.set_filters(Some(&filters));
                dialog.set_default_filter(Some(&filter));
                let window = window.clone();
                let row = row.clone();
                gtk::glib::spawn_future_local(async move {
                    let Ok(file) = dialog.open_future(Some(&window)).await else {
                        return;
                    };
                    if let Some(path) = file.path() {
                        row.set_text(&path.to_string_lossy().into_owned());
                    }
                });
            });
        }

        let media_folder_row = adw::EntryRow::new();
        media_folder_row.set_title(&tr("Media folder (optional)"));
        media_folder_row.set_text(&self.draft.borrow().media_folder);
        {
            let draft = Rc::clone(&self.draft);
            media_folder_row.connect_changed(move |row| {
                draft.borrow_mut().media_folder = row.text().to_string();
            });
        }
        let media_folder_browse = text_button("folder-open-symbolic", "Browse…");
        media_folder_row.add_suffix(&media_folder_browse);
        {
            let window = shell.window.clone();
            let row = media_folder_row.clone();
            media_folder_browse.connect_clicked(move |_| {
                let dialog = gtk::FileDialog::builder()
                    .title(tr("Choose the Media Folder"))
                    .build();
                let window = window.clone();
                let row = row.clone();
                gtk::glib::spawn_future_local(async move {
                    let Ok(folder) = dialog.select_folder_future(Some(&window)).await else {
                        return;
                    };
                    if let Some(path) = folder.path() {
                        row.set_text(&path.to_string_lossy().into_owned());
                    }
                });
            });
        }

        group.add(&library_row);
        group.add(&media_folder_row);
        content.append(&group);

        update_ready();
        actions.append(&connect);
        content.append(&actions);
        content.append(&status);

        let draft = Rc::clone(&self.draft);
        *context.actions.borrow_mut() = Some(SetupActions {
            status: status.clone(),
            connect: connect.clone(),
            ready: Rc::new(move || !draft.borrow().library.trim().is_empty()),
        });

        let source = shell.products.source.clone();
        let draft = Rc::clone(&self.draft);
        let status_for_login = status.clone();
        connect.connect_clicked(move |login| {
            let library = draft.borrow().library.trim().to_string();
            let media_folder = {
                let value = draft.borrow().media_folder.trim().to_string();
                (!value.is_empty()).then_some(value)
            };
            if library.is_empty() {
                status_for_login.set_text(&tr("Choose the iTunes Library.itl file"));
                status_for_login.set_visible(true);
                return;
            }
            begin_connect_attempt(&status_for_login, login, &tr("Reading iTunes library..."));
            let (source, status, login) =
                (source.clone(), status_for_login.downgrade(), login.downgrade());
            gtk::glib::spawn_future_local(async move {
                let result = source.save_itunes(None, library, media_folder, None).await;
                if let Err(error) = result {
                    if let Some(status) = status.upgrade() {
                        status.set_text(&error.to_string());
                        status.set_visible(true);
                    }
                    if let Some(login) = login.upgrade() {
                        login.set_sensitive(true);
                    }
                }
            });
        });

        scroller.upcast()
    }
}

/// The source settings form (edit an existing iTunes source): the same
/// library / media-folder pair as the setup flow, saved back through
/// `save_itunes(Some(id), ...)`. The timezone offset stays untouched —
/// the sync layer falls back to the offset in the `.itl` envelope, which is
/// what a fresh setup stores too.
pub(super) fn settings_group(
    shell: &Rc<Preferences>,
    saved: &EditableSource,
    _presentation: &'static SourcePresentation,
) -> Result<gtk::Widget, String> {
    let initial = saved
        .itunes_settings
        .clone()
        .unwrap_or_default();
    let draft = Rc::new(RefCell::new(Draft {
        library: initial.library,
        media_folder: initial.media_folder.unwrap_or_default(),
    }));

    let status = gtk::Label::new(None);
    status.set_wrap(true);
    status.set_visible(false);

    let save = gtk::Button::with_label(&tr("Save"));
    save.add_css_class("suggested-action");
    save.set_halign(gtk::Align::End);
    let update_ready: Rc<dyn Fn()> = Rc::new({
        let save = save.downgrade();
        let draft = Rc::clone(&draft);
        move || {
            if let Some(save) = save.upgrade() {
                save.set_sensitive(!draft.borrow().library.trim().is_empty());
            }
        }
    });

    let section = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let group = adw::PreferencesGroup::new();
    group.set_title(&tr("iTunes library"));
    group.set_description(Some(&tr(
        "The iTunes Library.itl file. Ratings and play counts sync back into it. Do not run iTunes on this library at the same time. The media folder root defaults to the folder containing the library file.",
    )));

    let library_row = adw::EntryRow::new();
    library_row.set_title(&tr("Library file"));
    library_row.set_text(&draft.borrow().library);
    library_row.set_input_purpose(gtk::InputPurpose::Url);
    {
        let draft = Rc::clone(&draft);
        let update_ready = Rc::clone(&update_ready);
        library_row.connect_changed(move |row| {
            draft.borrow_mut().library = row.text().to_string();
            update_ready();
        });
    }
    let library_browse = text_button("folder-open-symbolic", "Browse…");
    library_row.add_suffix(&library_browse);
    {
        let window = shell.window.clone();
        let row = library_row.clone();
        library_browse.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder()
                .title(tr("Choose the iTunes Library"))
                .build();
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("iTunes Library (.itl)"));
            filter.add_pattern("*.itl");
            let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            dialog.set_filters(Some(&filters));
            dialog.set_default_filter(Some(&filter));
            let window = window.clone();
            let row = row.clone();
            gtk::glib::spawn_future_local(async move {
                let Ok(file) = dialog.open_future(Some(&window)).await else {
                    return;
                };
                if let Some(path) = file.path() {
                    row.set_text(&path.to_string_lossy().into_owned());
                }
            });
        });
    }

    let media_folder_row = adw::EntryRow::new();
    media_folder_row.set_title(&tr("Media folder (optional)"));
    media_folder_row.set_text(&draft.borrow().media_folder);
    {
        let draft = Rc::clone(&draft);
        media_folder_row.connect_changed(move |row| {
            draft.borrow_mut().media_folder = row.text().to_string();
        });
    }
    let media_folder_browse = text_button("folder-open-symbolic", "Browse…");
    media_folder_row.add_suffix(&media_folder_browse);
    {
        let window = shell.window.clone();
        let row = media_folder_row.clone();
        media_folder_browse.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder()
                .title(tr("Choose the Media Folder"))
                .build();
            let window = window.clone();
            let row = row.clone();
            gtk::glib::spawn_future_local(async move {
                let Ok(folder) = dialog.select_folder_future(Some(&window)).await else {
                    return;
                };
                if let Some(path) = folder.path() {
                    row.set_text(&path.to_string_lossy().into_owned());
                }
            });
        });
    }

    group.add(&library_row);
    group.add(&media_folder_row);
    section.append(&group);
    section.append(&status);
    section.append(&save);

    update_ready();
    let source = shell.products.source.clone();
    let source_id = saved.source.id.clone();
    let draft_for_save = Rc::clone(&draft);
    let status_for_save = status.downgrade();
    let save_for_save = save.downgrade();
    save.connect_clicked(move |_| {
        let library = draft_for_save.borrow().library.trim().to_string();
        let media_folder = {
            let value = draft_for_save.borrow().media_folder.trim().to_string();
            (!value.is_empty()).then_some(value)
        };
        if library.is_empty() {
            if let Some(status) = status_for_save.upgrade() {
                status.set_text(&tr("Choose the iTunes Library.itl file"));
                status.set_visible(true);
            }
            return;
        }
        let (source, status, save, source_id) = (
            source.clone(),
            status_for_save.clone(),
            save_for_save.clone(),
            source_id.clone(),
        );
        if let Some(save) = save.upgrade() {
            save.set_sensitive(false);
        }
        gtk::glib::spawn_future_local(async move {
            let result = source
                .save_itunes(Some(source_id), library, media_folder, None)
                .await;
            if let Err(error) = result {
                if let Some(status) = status.upgrade() {
                    status.set_text(&error.to_string());
                    status.set_visible(true);
                }
                if let Some(save) = save.upgrade() {
                    save.set_sensitive(true);
                }
            }
        });
    });

    Ok(section.upcast())
}
