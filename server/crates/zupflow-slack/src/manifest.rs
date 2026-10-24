//! A generated app's Slack manifest, built from Zupflow's [`AppManifest`]
//! (which is built from the agent, never from a request's text), and its
//! digest: what an update compares, so an unchanged app isn't sent again.
//!
//! The app is a bot user named after the agent's handle, with the bot
//! scopes and events Zupflow asks for, its events delivered to its own
//! inbound URL over HTTP (no Socket Mode), its OAuth redirect on hosted
//! Connect, and no token rotation for its bot token. It has no icon: the
//! customer sets one in Slack.
//!
//! **Made in two steps.** Slack checks an events `request_url` the moment
//! a manifest names it, with a `url_verification` signed by the app's own
//! signing secret, which Zupflow only has once `apps.manifest.create` has
//! answered. So an app is made from [`slack_manifest_unsubscribed`] (no
//! `event_subscriptions` at all), its secrets stored, and only then sent
//! the whole manifest with `apps.manifest.update`, whose challenge the
//! inbound endpoint answers with the stored secret.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zupflow_core::channels::provision::AppManifest;

/// The longest description Slack shows for an app.
const DESCRIPTION_MAX: usize = 140;
/// The longest bot user display name Slack takes.
const DISPLAY_NAME_MAX: usize = 80;

/// The manifest Slack is sent for `manifest`. Its keys are sorted when
/// written (`serde_json` keeps maps sorted), so the same app is the same
/// bytes every time.
pub fn slack_manifest(manifest: &AppManifest) -> Value {
    let description: String = manifest.description.trim().chars().take(DESCRIPTION_MAX).collect();
    let mut display = json!({ "name": manifest.name });
    if !description.is_empty() {
        display["description"] = json!(description);
    }
    let display_name: String = manifest.handle.chars().take(DISPLAY_NAME_MAX).collect();
    // No `assistant_view`: it turns the app's DMs into assistant threads,
    // which need more subscriptions and plans than an agent's app asks for.
    // The "working…" status is best effort without it.
    json!({
        "display_information": display,
        "features": {
            "app_home": {
                "home_tab_enabled": false,
                "messages_tab_enabled": true,
                "messages_tab_read_only_enabled": false,
            },
            "bot_user": { "display_name": display_name, "always_online": true },
        },
        "oauth_config": {
            "redirect_urls": [manifest.redirect_url],
            "scopes": { "bot": manifest.scopes },
        },
        "settings": {
            "event_subscriptions": {
                "request_url": manifest.events_url,
                "bot_events": manifest.events,
            },
            "org_deploy_enabled": false,
            "socket_mode_enabled": false,
            "token_rotation_enabled": false,
        },
    })
}

/// The manifest an app is made from: [`slack_manifest`] without its
/// `event_subscriptions`, which are sent once the app's signing secret is
/// stored (see the module's notes).
pub fn slack_manifest_unsubscribed(manifest: &AppManifest) -> Value {
    let mut sent = slack_manifest(manifest);
    if let Some(settings) = sent.get_mut("settings").and_then(Value::as_object_mut) {
        settings.remove("event_subscriptions");
    }
    sent
}

/// What Slack is sent for `manifest`, as the JSON string its `manifest`
/// argument takes.
pub fn slack_manifest_text(manifest: &AppManifest) -> String {
    slack_manifest(manifest).to_string()
}

/// The manifest's digest: 64 hex characters of SHA-256 over what Slack is
/// sent: the same app is the same bytes, so the same digest.
pub fn digest(manifest: &AppManifest) -> String {
    Sha256::digest(slack_manifest_text(manifest).as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The manifest `apps.manifest.validate` checks a configuration token with:
/// the least Slack accepts. Nothing is made with it.
pub(crate) fn probe() -> String {
    json!({ "display_information": { "name": "Zupflow" } }).to_string()
}
