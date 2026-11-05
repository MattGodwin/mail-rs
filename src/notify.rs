//! Desktop notifications. Shown from a background thread so a slow
//! notification service never stalls mail sync.

pub fn show(title: String, body: String, on_click: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        let mut n = notify_rust::Notification::new();
        n.summary(&title).body(&body).appname("Mail");
        #[cfg(windows)]
        {
            if let Ok(handle) = n.show() {
                handle
                    .wait_for_response(|r: &notify_rust::NotificationResponse| {
                        if r.is_default_action() {
                            on_click();
                        }
                    })
                    .ok();
            }
        }
        #[cfg(not(windows))]
        {
            let _ = n.show();
            let _ = on_click;
        }
    });
}
