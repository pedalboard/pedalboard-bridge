//! Integration tests for the mod-host client.
//!
//! These tests require a running mod-host instance on localhost:5555
//! with an AIDA-X plugin loaded as instance 0.
//!
//! Run with: cargo test -- --ignored

use pedalboard_bridge::ModHostClient;

/// Connect and query CPU load.
#[tokio::test]
#[ignore = "requires mod-host on localhost:5555"]
async fn test_cpu_load() {
    let mut client = ModHostClient::connect("localhost:5555").await.unwrap();
    let load = client.cpu_load().await.unwrap();
    assert!(load >= 0.0 && load <= 100.0, "unexpected cpu load: {load}");
}

/// Get and set a parameter.
#[tokio::test]
#[ignore = "requires mod-host on localhost:5555"]
async fn test_param_get_set() {
    let mut client = ModHostClient::connect("localhost:5555").await.unwrap();

    client.param_set(0, "PREGAIN", 0.42).await.unwrap();
    let val = client.param_get(0, "PREGAIN").await.unwrap();
    assert!(
        (val - 0.42).abs() < 0.01,
        "expected PREGAIN ~0.42, got {val}"
    );
}

/// Load a preset bundle and switch models.
#[tokio::test]
#[ignore = "requires mod-host on localhost:5555"]
async fn test_preset_load() {
    let mut client = ModHostClient::connect("localhost:5555").await.unwrap();

    // Register the test preset bundle.
    client
        .bundle_add("/tmp/aidax-preset-test.lv2")
        .await
        .unwrap();

    // Load california-clean preset.
    client
        .preset_load(0, "http://pedalboard.local/presets#california-clean")
        .await
        .unwrap();

    let pregain = client.param_get(0, "PREGAIN").await.unwrap();
    assert!(
        (pregain - 0.5).abs() < 0.01,
        "expected PREGAIN 0.5, got {pregain}"
    );

    // Switch to british-rhythm.
    client
        .preset_load(0, "http://pedalboard.local/presets#british-rhythm")
        .await
        .unwrap();

    let pregain = client.param_get(0, "PREGAIN").await.unwrap();
    assert!(
        (pregain - 0.9).abs() < 0.01,
        "expected PREGAIN 0.9, got {pregain}"
    );

    // Clean up.
    client
        .bundle_remove("/tmp/aidax-preset-test.lv2")
        .await
        .unwrap();
}

/// Invalid parameter returns an error.
#[tokio::test]
#[ignore = "requires mod-host on localhost:5555"]
async fn test_invalid_param() {
    let mut client = ModHostClient::connect("localhost:5555").await.unwrap();
    let result = client.param_get(0, "NONEXISTENT").await;
    assert!(result.is_err());
}
