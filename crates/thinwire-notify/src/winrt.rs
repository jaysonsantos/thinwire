//! Windows: toast notifications through WinRT (#161).
//!
//! Each chat has one toast tag in the `thinwire` group. A new message
//! replaces the toast, a dismiss removes it, and a click opens the chat.
//! An app with no installer must register its AppUserModelID, so the start
//! writes one key under HKCU (no admin).

use std::collections::HashMap;
use std::sync::Arc;

use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::core::{HSTRING, h};

use crate::tagged::TagService;
use crate::{BackendError, ClickFn, Notification};

/// AppUserModelID of thinwire: the macOS bundle id too (ADR 0003).
const APP_ID: &str = "dev.jaysonsantos.thinwire";

/// The app's own key. Windows shows `DisplayName` on each toast.
const APP_ID_KEY: &str = r"Software\Classes\AppUserModelId\dev.jaysonsantos.thinwire";
const DISPLAY_NAME: &str = "Thinwire";

/// Group of every thinwire toast.
const GROUP: &str = "thinwire";

pub(crate) struct Toasts {
    clicks: ClickFn,
    /// The shown toast of each tag. Its click handler lives while the
    /// toast lives here.
    shown: HashMap<String, ToastNotification>,
}

impl Toasts {
    /// Runs on the notification thread, not on the UI thread.
    pub(crate) fn new(clicks: ClickFn) -> Self {
        if windows_registry::CURRENT_USER
            .create(APP_ID_KEY)
            .and_then(|key| key.set_string("DisplayName", DISPLAY_NAME))
            .is_err()
        {
            tracing::warn!("could not register the notification app id");
        }
        // Toasts of an earlier run that did not end cleanly: no handler
        // opens their chat now.
        let cleared = ToastNotificationManager::History()
            .and_then(|history| history.RemoveGroupWithId(&group(), &app_id()));
        if cleared.is_err() {
            tracing::debug!("could not remove old toasts");
        }
        Self {
            clicks,
            shown: HashMap::new(),
        }
    }
}

fn app_id() -> HSTRING {
    HSTRING::from(APP_ID)
}

fn group() -> HSTRING {
    HSTRING::from(GROUP)
}

/// Toast content: the chat title and the body line. `quiet`: no sound.
/// The DOM escapes the text.
fn content(notification: &Notification, quiet: bool) -> windows::core::Result<XmlDocument> {
    let doc = XmlDocument::new()?;
    let toast = doc.CreateElement(h!("toast"))?;
    let visual = doc.CreateElement(h!("visual"))?;
    let binding = doc.CreateElement(h!("binding"))?;
    binding.SetAttribute(h!("template"), h!("ToastGeneric"))?;
    for line in [notification.title.clone(), notification.body()] {
        let text = doc.CreateElement(h!("text"))?;
        text.SetInnerText(&HSTRING::from(line))?;
        binding.AppendChild(&text)?;
    }
    visual.AppendChild(&binding)?;
    toast.AppendChild(&visual)?;
    if quiet {
        let audio = doc.CreateElement(h!("audio"))?;
        audio.SetAttribute(h!("silent"), h!("true"))?;
        toast.AppendChild(&audio)?;
    }
    doc.AppendChild(&toast)?;
    Ok(doc)
}

impl TagService for Toasts {
    fn post(
        &mut self,
        tag: &str,
        notification: &Notification,
        quiet: bool,
    ) -> Result<(), BackendError> {
        let toast = content(notification, quiet)
            .and_then(|doc| ToastNotification::CreateToastNotification(&doc))
            .map_err(|_| BackendError("toast content"))?;
        let clicks = Arc::clone(&self.clicks);
        let key = notification.key.clone();
        let setup = toast
            .SetTag(&HSTRING::from(tag))
            .and_then(|()| toast.SetGroup(&group()))
            .and_then(|()| toast.SetSuppressPopup(quiet))
            .and_then(|()| {
                // A click on the toast body, also from the notification
                // center. It runs on a WinRT thread.
                toast.Activated(&TypedEventHandler::new(move |_, _| {
                    clicks(key.clone());
                    Ok(())
                }))
            });
        setup.map_err(|_| BackendError("toast setup"))?;
        ToastNotificationManager::CreateToastNotifierWithId(&app_id())
            .and_then(|notifier| notifier.Show(&toast))
            .map_err(|_| BackendError("show"))?;
        self.shown.insert(tag.to_owned(), toast);
        Ok(())
    }

    fn has(&mut self, tag: &str) -> Result<bool, BackendError> {
        let listed = ToastNotificationManager::History()
            .and_then(|history| history.GetHistoryWithId(&app_id()))
            .map_err(|_| BackendError("toast history"))?;
        Ok(listed
            .into_iter()
            .any(|toast| toast.Tag().is_ok_and(|shown| shown == tag)))
    }

    fn remove(&mut self, tag: &str) -> Result<(), BackendError> {
        ToastNotificationManager::History()
            .and_then(|history| {
                history.RemoveGroupedTagWithId(&HSTRING::from(tag), &group(), &app_id())
            })
            .map_err(|_| BackendError("remove toast"))
    }

    fn forget(&mut self, tag: &str) {
        self.shown.remove(tag);
    }
}
