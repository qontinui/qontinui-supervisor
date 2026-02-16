use std::path::PathBuf;
use std::sync::Arc;

use qontinui_supervisor::config::SupervisorConfig;
use qontinui_supervisor::health_cache::CachedPortHealth;
use qontinui_supervisor::state::SupervisorState;

fn test_config() -> SupervisorConfig {
    SupervisorConfig {
        project_dir: PathBuf::from("/tmp/test/src-tauri"),
        dev_mode: true,
        watchdog_enabled_at_start: false,
        auto_start: false,
        auto_debug: false,
        log_file: None,
        port: 9875,
        dev_logs_dir: PathBuf::from("/tmp/.dev-logs"),
        cli_args: vec![],
        expo_dir: None,
        expo_port: 8081,
    }
}

#[tokio::test]
async fn test_state_creation() {
    let state = SupervisorState::new(test_config());

    // Runner starts as not running
    let runner = state.runner.read().await;
    assert!(!runner.running);
    assert!(runner.pid.is_none());
    assert!(runner.started_at.is_none());
    drop(runner);

    // Watchdog starts disabled (per config)
    let watchdog = state.watchdog.read().await;
    assert!(!watchdog.enabled);
    assert_eq!(watchdog.restart_attempts, 0);
    assert!(watchdog.crash_history.is_empty());
    drop(watchdog);

    // Build starts clean
    let build = state.build.read().await;
    assert!(!build.build_in_progress);
    assert!(!build.build_error_detected);
    drop(build);

    // AI starts with defaults
    let ai = state.ai.read().await;
    assert!(!ai.running);
    assert_eq!(ai.provider, "claude");
    assert_eq!(ai.model, "opus");
    assert!(!ai.auto_debug_enabled);
    drop(ai);

    // Expo starts as not running
    let expo = state.expo.read().await;
    assert!(!expo.running);
    assert_eq!(expo.port, 8081);
}

#[tokio::test]
async fn test_notify_health_change() {
    let state = Arc::new(SupervisorState::new(test_config()));
    let mut rx = state.health_tx.subscribe();

    // Sending should not panic even with a subscriber
    state.notify_health_change();

    let result = rx.try_recv();
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_health_broadcast_multiple_receivers() {
    let state = Arc::new(SupervisorState::new(test_config()));
    let mut rx1 = state.health_tx.subscribe();
    let mut rx2 = state.health_tx.subscribe();

    state.notify_health_change();

    assert!(rx1.try_recv().is_ok());
    assert!(rx2.try_recv().is_ok());
}

#[tokio::test]
async fn test_cached_health_default() {
    let state = SupervisorState::new(test_config());
    let cached = state.cached_health.read().await;

    assert!(!cached.runner_port_open);
    assert!(!cached.runner_responding);
    assert!(!cached.vite_port_open);
}

#[test]
fn test_cached_port_health_clone() {
    let health = CachedPortHealth {
        runner_port_open: true,
        runner_responding: true,
        vite_port_open: false,
    };

    let cloned = health.clone();
    assert!(cloned.runner_port_open);
    assert!(cloned.runner_responding);
    assert!(!cloned.vite_port_open);
}

#[tokio::test]
async fn test_cached_port_health_update() {
    let state = SupervisorState::new(test_config());

    // Default is all false
    {
        let cached = state.cached_health.read().await;
        assert!(!cached.runner_responding);
    }

    // Update the cache
    {
        let mut cached = state.cached_health.write().await;
        *cached = CachedPortHealth {
            runner_port_open: true,
            runner_responding: true,
            vite_port_open: true,
        };
    }

    // Read back updated values
    {
        let cached = state.cached_health.read().await;
        assert!(cached.runner_port_open);
        assert!(cached.runner_responding);
        assert!(cached.vite_port_open);
    }
}

#[tokio::test]
async fn test_ai_output_buffer() {
    let state = SupervisorState::new(test_config());
    let mut ai = state.ai.write().await;

    ai.push_output("stdout", "test line 1".to_string());
    ai.push_output("stderr", "test error".to_string());

    assert_eq!(ai.output_buffer.len(), 2);
    assert_eq!(ai.output_buffer[0].stream, "stdout");
    assert_eq!(ai.output_buffer[0].line, "test line 1");
    assert_eq!(ai.output_buffer[1].stream, "stderr");
    assert_eq!(ai.output_buffer[1].line, "test error");
}

#[tokio::test]
async fn test_watchdog_crash_recording() {
    let state = SupervisorState::new(test_config());
    let mut watchdog = state.watchdog.write().await;

    assert!(watchdog.crash_history.is_empty());

    watchdog.record_crash();
    assert_eq!(watchdog.crash_history.len(), 1);

    watchdog.record_crash();
    assert_eq!(watchdog.crash_history.len(), 2);

    // Should not be a crash loop with only 2 crashes (threshold is 5)
    assert!(!watchdog.is_crash_loop(5, 600));
}

#[tokio::test]
async fn test_health_cache_notify() {
    let state = Arc::new(SupervisorState::new(test_config()));
    let state_clone = state.clone();

    // Spawn a task that waits for notification
    let handle = tokio::spawn(async move {
        state_clone.health_cache_notify.notified().await;
        true
    });

    // Give spawned task time to start waiting
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Notify should wake the task
    state.health_cache_notify.notify_one();

    let result = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
    assert!(result.is_ok());
    assert!(result.unwrap().unwrap());
}

#[tokio::test]
async fn test_watchdog_cooldown() {
    let state = SupervisorState::new(test_config());
    let mut watchdog = state.watchdog.write().await;

    // No last restart — should not be in cooldown
    assert!(!watchdog.is_in_cooldown(60));

    // Set last restart to now
    watchdog.last_restart_at = Some(chrono::Utc::now());
    // Should be in cooldown with 60s window
    assert!(watchdog.is_in_cooldown(60));

    // Should not be in cooldown with 0s window
    assert!(!watchdog.is_in_cooldown(0));
}

#[tokio::test]
async fn test_watchdog_crash_loop_detection() {
    let state = SupervisorState::new(test_config());
    let mut watchdog = state.watchdog.write().await;

    // Record exactly threshold-1 crashes — should NOT be a crash loop
    for _ in 0..4 {
        watchdog.record_crash();
    }
    assert!(!watchdog.is_crash_loop(5, 600));

    // One more crash (at threshold) — SHOULD be a crash loop
    watchdog.record_crash();
    assert!(watchdog.is_crash_loop(5, 600));

    // Beyond threshold — still a crash loop
    watchdog.record_crash();
    assert!(watchdog.is_crash_loop(5, 600));
}
