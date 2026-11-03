//! Shared by the test targets that start belay processes.

/// Keep the tests' belay processes off the user's desktop. They run with the default
/// `notify = attention`, so with a real display each fake session's block, loop, or
/// background stop would pop up on the screen of whoever runs `make check`.
pub fn no_desktop() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::remove_var("DISPLAY");
        std::env::remove_var("WAYLAND_DISPLAY");
        // Unset, D-Bus clients fall back to $XDG_RUNTIME_DIR/bus; this address fails.
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", "disabled:");
    });
}
