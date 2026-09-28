//! Toast notifications, from an unpackaged app.
//!
//! A toast names its app by id ([`APP_ID`]), whose name and icon Kith puts
//! in the registry (`install::register`). Its buttons open `kith://` links
//! (protocol activation), which need no COM server: Windows runs
//! `kith.exe open <link>`, and that hands the link to the running Kith.

use anyhow::{Context, Result};
use windows::{
    Data::Xml::Dom::XmlDocument,
    UI::Notifications::{ToastNotification, ToastNotificationManager},
    Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx},
    core::HSTRING,
};

use crate::{install::APP_ID, link::Link};

/// Shows a toast. With `watch`, its Watch button and a click on the toast
/// itself open that link.
pub async fn toast(title: &str, body: &str, watch: Option<&Link>) -> Result<()> {
    let xml = toast_xml(title, body, watch);
    tokio::task::spawn_blocking(move || show(&xml))
        .await
        .context("the notification thread panicked")?
}

fn show(xml: &str) -> Result<()> {
    // SAFETY: joins this thread to the multithreaded apartment, which WinRT
    // needs; doing it twice on a reused thread is harmless.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let document = XmlDocument::new().context("creating the toast")?;
    document
        .LoadXml(&HSTRING::from(xml))
        .context("writing the toast")?;
    let toast =
        ToastNotification::CreateToastNotification(&document).context("creating the toast")?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))
        .context("reaching Windows' notifications")?
        .Show(&toast)
        .context("showing the toast")
}

fn toast_xml(title: &str, body: &str, watch: Option<&Link>) -> String {
    let (launch, actions) = match watch {
        Some(link) => {
            let link = escape(&link.to_string());
            (
                format!(r#" launch="{link}" activationType="protocol""#),
                format!(
                    r#"<actions><action content="Watch" activationType="protocol" arguments="{link}"/></actions>"#
                ),
            )
        }
        None => (String::new(), String::new()),
    };
    format!(
        r#"<toast{launch}><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual>{actions}</toast>"#,
        escape(title),
        escape(body)
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toasts_link_their_watch_button() {
        let code = "9f79d3dea4117e8337f2780e871a3d10b2b1106ac655f97447f78b090b6c58c3";
        let link: Link = format!("kith://watch/{code}").parse().unwrap();
        let xml = toast_xml("<sam> is live", "Watch opens the stream.", Some(&link));
        assert!(xml.contains("<text>&lt;sam&gt; is live</text>"), "{xml}");
        assert!(
            xml.contains(&format!(r#"arguments="kith://watch/{code}""#)),
            "{xml}"
        );
        assert!(xml.starts_with(&format!(
            r#"<toast launch="kith://watch/{code}" activationType="protocol">"#
        )));
        assert!(!toast_xml("hi", "there", None).contains("actions"));
    }
}
