//! #161 spike: check UNUserNotificationCenter inside an ad-hoc signed
//! `Thinwire.app`. Each step prints one `STEP <name>: <result>` line.
//!
//! Run it from `Thinwire.app/Contents/MacOS/`. Set `PROBE_WAIT_CLICK` to a
//! number of seconds to wait for a click on the last notification.

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("STEP platform: not macOS, nothing to probe");
}

#[cfg(target_os = "macos")]
fn main() {
    probe::run();
}

#[cfg(target_os = "macos")]
mod probe {
    use std::future::Future;
    use std::task::Poll;
    use std::time::{Duration, Instant};

    use mac_usernotifications as un;

    const TAG: &str = "thinwire-probe";

    /// Run `future` on the main run loop for at most `limit`.
    fn within<T>(limit: Duration, future: impl Future<Output = T>) -> Option<T> {
        let deadline = Instant::now() + limit;
        let mut future = Box::pin(future);
        // `block_on_main` polls again at least once a second.
        un::block_on_main(std::future::poll_fn(move |cx| {
            if let Poll::Ready(value) = future.as_mut().poll(cx) {
                return Poll::Ready(Some(value));
            }
            if Instant::now() >= deadline {
                return Poll::Ready(None);
            }
            Poll::Pending
        }))
    }

    fn step(name: &str, result: impl std::fmt::Debug) {
        println!("STEP {name}: {result:?}");
    }

    fn delivered() -> Option<Vec<String>> {
        within(
            Duration::from_secs(10),
            un::get_delivered_notification_ids(),
        )
    }

    pub(super) fn run() {
        let bundle = un::check_bundle();
        step("bundle", &bundle);
        if bundle.is_err() {
            println!("STEP result: no bundle id, the app keeps the show-only backend");
            return;
        }
        step(
            "settings-before",
            within(Duration::from_secs(10), un::get_notification_settings()),
        );
        step("auth", within(Duration::from_secs(30), un::request_auth()));
        step(
            "settings-after",
            within(Duration::from_secs(10), un::get_notification_settings()),
        );

        let first = within(
            Duration::from_secs(10),
            un::Notification::new()
                .id(TAG)
                .title("thinwire probe")
                .message("first message")
                .default_sound()
                .send(),
        );
        step("send", first.as_ref().map(|sent| sent.as_ref().map(|_| ())));
        std::thread::sleep(Duration::from_secs(2));
        step("delivered-after-send", delivered());

        let second = within(
            Duration::from_secs(10),
            un::Notification::new()
                .id(TAG)
                .title("thinwire probe")
                .message("second message, quiet replace")
                .interruption_level(un::InterruptionLevel::Passive)
                .send(),
        );
        step(
            "replace",
            second.as_ref().map(|sent| sent.as_ref().map(|_| ())),
        );
        std::thread::sleep(Duration::from_secs(2));
        step("delivered-after-replace", delivered());

        un::blocking::close_delivered(TAG);
        std::thread::sleep(Duration::from_secs(2));
        step("delivered-after-remove", delivered());

        let wait = std::env::var("PROBE_WAIT_CLICK")
            .ok()
            .and_then(|secs| secs.parse().ok())
            .unwrap_or(0);
        if wait > 0 {
            let clicked = within(Duration::from_secs(wait), async {
                match un::Notification::new()
                    .id(TAG)
                    .title("thinwire probe")
                    .message("click this notification")
                    .send()
                    .await
                {
                    Ok(handle) => handle.response().await,
                    Err(error) => Err(error),
                }
            });
            step(
                "click",
                clicked.map(|response| response.map(|response| response.is_default_action())),
            );
            un::blocking::close_delivered(TAG);
        }
        println!("STEP done");
    }
}
