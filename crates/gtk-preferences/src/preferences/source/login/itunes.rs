//! iTunes source setup: pick the `iTunes Library.itl` and an optional
//! media-folder root override (the local root of the guest "Music Folder";
//! defaults to the parent directory of the `.itl`).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use super::{begin_connect_attempt, setup_scaffold, SetupActions, SetupViewContext, SourcePresentation, SourceSetupFlow};
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
            library_row.connect_changed(move |row| {
                draft.borrow_mut().library = row.text().to_string();
            });
        }
        let library_browse = text_button("folder-open-symbolic", "Browse…");
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
        let library_picker = adw::ActionRow::new();
        library_picker.set_title(&tr("iTunes Library.itl"));
        library_picker.set_subtitle(&tr("Usually inside the iTunes folder"));
        library_picker.add_suffix(&library_browse);
        library_picker.set_activatable_widget(Some(&library_browse));

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
        let media_folder_picker = adw::ActionRow::new();
        media_folder_picker.add_suffix(&media_folder_browse);
        media_folder_picker.set_activatable_widget(Some(&media_folder_browse));

        group.add(&library_row);
        group.add(&library_picker);
        group.add(&media_folder_row);
        group.add(&media_folder_picker);
        content.append(&group);

        let connect = text_button("rufin-network-server-symbolic", "Connect");
        connect.add_css_class("suggested-action");
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
