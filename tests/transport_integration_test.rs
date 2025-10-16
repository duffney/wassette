// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.
#![allow(clippy::uninlined_format_args)]

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::Full;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use oci_wasm::WasmClient;
use tempfile::TempDir;
use test_log::test;
use testcontainers::core::WaitFor;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, Image};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::time::sleep;
use wassette::LifecycleManager;

mod common;
use common::build_fetch_component;

const DOCKER_REGISTRY_PORT: u16 = 5000;

pub async fn find_open_port() -> Result<u16> {
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .await
        .context("failed to bind random port")?
        .local_addr()
        .map(|addr| addr.port())
        .context("failed to get local address from opened TCP socket")
}

#[derive(Default)]
struct DockerRegistry {
    _priv: (),
}

impl Image for DockerRegistry {
    fn name(&self) -> &str {
        "registry"
    }

    fn tag(&self) -> &str {
        "2"
    }

    fn ready_conditions(&self) -> Vec<WaitFor> {
        vec![WaitFor::message_on_stderr("listening on")]
    }
}

async fn setup_registry() -> anyhow::Result<ContainerAsync<DockerRegistry>> {
    DockerRegistry::default()
        .start()
        .await
        .context("Failed to start docker registry")
}

async fn cleanup_components(manager: &LifecycleManager) -> Result<()> {
    let component_ids = manager.list_components().await;
    for id in component_ids {
        manager.unload_component(&id).await?;
    }
    Ok(())
}

async fn setup_lifecycle_manager() -> Result<(Arc<LifecycleManager>, TempDir)> {
    setup_lifecycle_manager_with_client(reqwest::Client::default()).await
}

async fn setup_lifecycle_manager_with_client(
    http_client: reqwest::Client,
) -> Result<(Arc<LifecycleManager>, TempDir)> {
    let tempdir = tempfile::tempdir()?;

    let manager = Arc::new(
        LifecycleManager::builder(tempdir.path())
            .with_environment_vars(std::collections::HashMap::new())
            .with_oci_client(oci_client::Client::new(oci_client::client::ClientConfig {
                protocol: oci_client::client::ClientProtocol::Http,
                ..Default::default()
            }))
            .with_http_client(http_client)
            .build()
            .await
            .context("Failed to create LifecycleManager")?,
    );

    cleanup_components(&manager).await?;

    Ok((manager, tempdir))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test(tokio::test)]
async fn test_fetch_component_workflow() -> Result<()> {
    // Start a local HTTP server to avoid relying on external network access
    let mock_html = b"<html><body><h1>Example Domain</h1><p>This domain is for use in documentation examples</p></body></html>";
    let (addr, _server_handle) = start_mock_http_server(mock_html.to_vec()).await?;
    let mock_url = format!("http://{addr}/");

    let (manager, _tempdir) = setup_lifecycle_manager().await?;

    let initial_components = manager.list_components().await;
    assert!(
        initial_components.is_empty(),
        "Expected no components initially"
    );

    let component_path = build_fetch_component().await?;

    let id = manager
        .load_component(&format!("file://{}", component_path.to_str().unwrap()))
        .await?
        .component_id;

    let components_after_load = manager.list_components().await;
    assert_eq!(components_after_load.len(), 1);
    assert_eq!(components_after_load[0], "fetch_rs");

    let schema = manager
        .get_component_schema(&id)
        .await
        .context("Component not found")?;
    assert!(schema["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "fetch"));

    // Grant permission for the local mock server (use IP address or localhost)
    let grant_result = manager
        .grant_permission(
            &id,
            "network",
            &serde_json::json!({"host": addr.ip().to_string()}),
        )
        .await;
    assert!(grant_result.is_ok(), "Failed to grant network permission");

    let result = manager
        .execute_component_call(&id, "fetch", &format!(r#"{{"url": "{}"}}"#, mock_url))
        .await?;

    let response_body = result;
    assert!(response_body.contains("Example Domain"));
    assert!(response_body.contains("This domain is for use in documentation examples"));

    // Copy the component to another name
    let mut component_path2 = component_path.clone();
    component_path2.set_file_name("fetch2.wasm");
    tokio::fs::copy(&component_path, &component_path2).await?;

    manager
        .load_component(&format!("file://{}", component_path2.to_str().unwrap()))
        .await?;

    // This should now fail because there are multiple components with the same tool
    let component_id_result = manager.get_component_id_for_tool("fetch").await;
    assert!(component_id_result.is_err());
    let error = component_id_result.unwrap_err();
    assert!(error
        .to_string()
        .contains("Multiple components found for tool 'fetch'"));
    assert!(error.to_string().contains("fetch_rs"));
    assert!(error.to_string().contains("fetch2"));

    Ok(())
}

async fn start_https_server(
    wasm_content: Vec<u8>,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    use rustls::pki_types::PrivateKeyDer;
    use tokio_rustls::{rustls, TlsAcceptor};

    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])?;
    let cert_der = cert.cert.der().clone();
    let key_bytes = cert.signing_key.serialize_der();
    let key_der = PrivateKeyDer::try_from(key_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to convert private key: {}", e))?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;

    let acceptor = TlsAcceptor::from(Arc::new(config));
    let wasm_bytes = Arc::new(wasm_content);

    let handle = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            let wasm_bytes = wasm_bytes.clone();

            tokio::spawn(async move {
                let tls_stream = match acceptor.accept(stream).await {
                    Ok(tls_stream) => tls_stream,
                    Err(e) => {
                        eprintln!("TLS handshake failed: {e:?}");
                        return;
                    }
                };

                let io = TokioIo::new(tls_stream);
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let wasm_bytes = wasm_bytes.clone();
                    async move {
                        if req.uri().path() != "/fetch_rs.wasm" {
                            return Ok::<_, hyper::Error>(
                                Response::builder()
                                    .status(404)
                                    .body(Full::new(Bytes::from("Not Found")))
                                    .unwrap(),
                            );
                        }
                        let response = Response::builder()
                            .status(200)
                            .header("Content-Type", "application/wasm")
                            .body(Full::new(Bytes::from(wasm_bytes.as_ref().clone())))
                            .unwrap();
                        Ok::<_, hyper::Error>(response)
                    }
                });

                if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                    eprintln!("Error serving connection: {err:?}");
                }
            });
        }
    });

    Ok((addr, handle))
}

async fn start_mock_http_server(
    content: Vec<u8>,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    let content_bytes = Arc::new(content);

    let handle = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let content_bytes = content_bytes.clone();

            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |_req: Request<hyper::body::Incoming>| {
                    let content_bytes = content_bytes.clone();
                    async move {
                        let response = Response::builder()
                            .status(200)
                            .header("Content-Type", "text/html")
                            .body(Full::new(Bytes::from(content_bytes.as_ref().clone())))
                            .unwrap();
                        Ok::<_, hyper::Error>(response)
                    }
                });

                if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                    eprintln!("Error serving connection: {err:?}");
                }
            });
        }
    });

    Ok((addr, handle))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test(tokio::test)]
async fn test_load_component_from_https() -> Result<()> {
    // Create HTTP client that ignores certificate validation for testing
    let http_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;

    let (manager, _tempdir) = setup_lifecycle_manager_with_client(http_client).await?;

    // Build the test component
    let component_path = build_fetch_component().await?;

    // Read the component bytes
    let wasm_bytes = tokio::fs::read(&component_path).await?;

    // Start HTTPS server
    let (addr, _server_handle) = start_https_server(wasm_bytes).await?;

    // Load from HTTPS
    let https_url = format!("https://{addr}/fetch_rs.wasm");
    let outcome = manager.load_component(&https_url).await?;
    let id = outcome.component_id.clone();

    // Verify component was loaded
    let components = manager.list_components().await;
    assert!(components.contains(&"fetch_rs".to_string()));

    // Test calling the component
    let result = manager
        .execute_component_call(&id, "fetch", r#"{"url": "https://example.com/"}"#)
        .await
        .context("Failed to execute component call")?;

    let response_body = result;
    assert!(!response_body.is_empty());

    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test(tokio::test)]
async fn test_load_component_from_oci() -> Result<()> {
    let (manager, _tempdir) = setup_lifecycle_manager().await?;

    // Build the test component
    let component_path = build_fetch_component().await?;

    // Start OCI registry using testcontainers - skip if Docker is not available
    let container = match setup_registry().await {
        Ok(container) => container,
        Err(e) => {
            let error_msg = e.to_string();
            if error_msg.contains("Socket not found")
                || error_msg.contains("docker client")
                || error_msg.contains("Failed to start docker registry")
            {
                println!("Skipping OCI test: Docker is not available - {error_msg}");
                return Ok(());
            }
            return Err(e);
        }
    };
    let registry_port = container.get_host_port_ipv4(DOCKER_REGISTRY_PORT).await?;
    let registry_url = format!("localhost:{registry_port}");

    // Give the registry a moment to fully start
    sleep(Duration::from_millis(500)).await;

    // Read component bytes
    let (config, layer) = oci_wasm::WasmConfig::from_component(component_path, None).await?;

    // Create OCI client and push the component
    let oci_client = oci_client::Client::new(oci_client::client::ClientConfig {
        protocol: oci_client::client::ClientProtocol::Http,
        ..Default::default()
    });

    let wasm_client = WasmClient::new(oci_client);
    let reference = format!("{registry_url}/fetch_rs:latest");
    let oci_reference: oci_client::Reference = reference.parse()?;

    // Push to registry
    wasm_client
        .push(
            &oci_reference,
            &oci_client::secrets::RegistryAuth::Anonymous,
            layer,
            config,
            None,
        )
        .await?;

    // Load from OCI
    let oci_url = format!("oci://{reference}");
    manager.load_component(&oci_url).await?;

    // Verify component was loaded
    let components = manager.list_components().await;
    assert!(components.contains(&"fetch_rs".to_string()));

    Ok(())
}

#[test(tokio::test)]
async fn test_load_component_invalid_scheme() -> Result<()> {
    let (manager, _tempdir) = setup_lifecycle_manager().await?;

    // Try to load with invalid scheme
    let result = manager
        .load_component("ftp://example.com/component.wasm")
        .await;
    assert!(result.is_err());
    let error = result.unwrap_err();
    assert!(error.to_string().contains("Unsupported component scheme"));

    Ok(())
}

#[test(tokio::test)]
async fn test_load_component_https_404() -> Result<()> {
    // Create HTTP client that ignores certificate validation for testing
    let http_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;

    let (manager, _tempdir) = setup_lifecycle_manager_with_client(http_client).await?;

    // Start HTTPS server
    let (addr, _server_handle) = start_https_server(Vec::new()).await?;

    // Try to load from HTTPS with 404
    let https_url = format!("https://{addr}/nonexistent.wasm");
    let result = manager.load_component(&https_url).await;
    assert!(result.is_err());
    let error = result.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Failed to download component from URL"),
        "Wrong error message found, got: {error}"
    );

    Ok(())
}

#[test(tokio::test)]
async fn test_load_component_invalid_reference() -> Result<()> {
    let (manager, _tempdir) = setup_lifecycle_manager().await?;

    // Try to load without scheme
    let result = manager.load_component("not_a_valid_reference").await;
    assert!(result.is_err());
    let error = result.unwrap_err();
    assert!(error.to_string().contains("Invalid component reference"));

    Ok(())
}
#[test(tokio::test)]
async fn test_mixed_transport_fails() -> Result<()> {
    // Create a temporary directory for this test to avoid loading existing components
    let temp_dir = tempfile::tempdir()?;
    let plugin_dir_arg = format!("--plugin-dir={}", temp_dir.path().display());

    // Get the path to the built binary
    let binary_path = std::env::current_dir()
        .context("Failed to get current directory")?
        .join("target/debug/wassette");

    let combinations = [
        ["--sse", "--stdio"],
        ["--stdio", "--streamable-http"],
        ["--sse", "--streamable-http"],
    ];

    for combo in combinations {
        // Start the server with the current combination of transports (should fail)
        let mut child = tokio::process::Command::new(&binary_path)
            .args(["serve", &plugin_dir_arg])
            .args(combo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to start wassette with mixed transports")?;

        let stderr = child.stderr.take().context("Failed to get stderr handle")?;
        let mut stderr = BufReader::new(stderr);

        // Give the server time to start
        tokio::time::sleep(Duration::from_millis(1000)).await;

        // Check if the process has exited
        let status = child
            .wait()
            .await
            .context("Failed to wait for wassette process")?;

        assert!(
            !status.success(),
            "Process should have exited with error due to mixed transports"
        );

        // Read stderr output
        let mut stderr_output = String::new();
        let _ = stderr.read_line(&mut stderr_output).await;

        let expected_error = format!(
            "the argument '{}' cannot be used with '{}'",
            combo[0], combo[1]
        );

        assert!(
            stderr_output.contains(&expected_error),
            "Expected error message about mixed transports, got: {stderr_output}"
        );
    }

    Ok(())
}

#[test(tokio::test)]
async fn test_stdio_transport() -> Result<()> {
    // Create a temporary directory for this test to avoid loading existing components
    let temp_dir = tempfile::tempdir()?;
    let plugin_dir_arg = format!("--plugin-dir={}", temp_dir.path().display());

    // Get the path to the built binary
    let binary_path = std::env::current_dir()
        .context("Failed to get current directory")?
        .join("target/debug/wassette");

    // Start the server with stdio transport (disable logs to avoid stdout pollution)
    let mut child = tokio::process::Command::new(&binary_path)
        .args(["serve", &plugin_dir_arg])
        .env("RUST_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start wassette with stdio transport")?;

    let stdin = child.stdin.take().context("Failed to get stdin handle")?;
    let stdout = child.stdout.take().context("Failed to get stdout handle")?;
    let stderr = child.stderr.take().context("Failed to get stderr handle")?;

    let mut stdin = stdin;
    let mut stdout = BufReader::new(stdout);
    let mut stderr = BufReader::new(stderr);

    // Give the server time to start (less time needed with empty plugin dir)
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Check if the process is still running
    if let Ok(Some(status)) = child.try_wait() {
        // Process has exited, read stderr to see what went wrong
        let mut stderr_output = String::new();
        let _ = stderr.read_line(&mut stderr_output).await;
        return Err(anyhow::anyhow!(
            "Server process exited with status: {:?}, stderr: {}",
            status,
            stderr_output
        ));
    }

    // Send MCP initialize request
    let initialize_request = r#"{"jsonrpc": "2.0", "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test-client", "version": "1.0.0"}}, "id": 1}
"#;

    stdin.write_all(initialize_request.as_bytes()).await?;
    stdin.flush().await?;

    // Read and verify response with longer timeout for component loading
    let mut response_line = String::new();
    match tokio::time::timeout(
        Duration::from_secs(10),
        stdout.read_line(&mut response_line),
    )
    .await
    {
        Ok(Ok(_)) => {
            // Successfully read a line
        }
        Ok(Err(e)) => {
            // Read error
            return Err(anyhow::anyhow!("Failed to read initialize response: {}", e));
        }
        Err(_) => {
            // Timeout - try to read stderr to see if there are any error messages
            let mut stderr_output = String::new();
            let _ =
                tokio::time::timeout(Duration::from_secs(1), stderr.read_line(&mut stderr_output))
                    .await;
            return Err(anyhow::anyhow!(
                "Timeout waiting for initialize response. Stderr: {}",
                stderr_output
            ));
        }
    }

    if response_line.trim().is_empty() {
        return Err(anyhow::anyhow!("Received empty response"));
    }

    let response: serde_json::Value =
        serde_json::from_str(&response_line).context("Failed to parse initialize response")?;

    // Verify the response structure
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert!(response["result"].is_object());
    assert_eq!(response["result"]["protocolVersion"], "2024-11-05");
    assert!(response["result"]["capabilities"]["tools"]["listChanged"]
        .as_bool()
        .unwrap_or(false));

    // Send initialized notification (required by MCP protocol)
    let initialized_notification = r#"{"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}
"#;

    stdin.write_all(initialized_notification.as_bytes()).await?;
    stdin.flush().await?;

    // Send list_tools request
    let list_tools_request = r#"{"jsonrpc": "2.0", "method": "tools/list", "params": {}, "id": 2}
"#;

    stdin.write_all(list_tools_request.as_bytes()).await?;
    stdin.flush().await?;

    // Read and verify tools list response
    let mut tools_response_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        stdout.read_line(&mut tools_response_line),
    )
    .await
    .context("Timeout waiting for tools/list response")?
    .context("Failed to read tools/list response")?;

    let tools_response: serde_json::Value = serde_json::from_str(&tools_response_line)
        .context("Failed to parse tools/list response")?;

    // Verify the tools response structure
    assert_eq!(tools_response["jsonrpc"], "2.0");
    assert_eq!(tools_response["id"], 2);
    assert!(tools_response["result"].is_object());
    assert!(tools_response["result"]["tools"].is_array());

    // Verify we have the expected built-in tools
    let tools = &tools_response["result"]["tools"].as_array().unwrap();
    assert!(tools.len() >= 2); // Should have at least load-component and unload-component

    let tool_names: Vec<String> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(tool_names.contains(&"load-component".to_string()));
    assert!(tool_names.contains(&"unload-component".to_string()));

    // Clean up
    child.kill().await.ok();

    Ok(())
}

#[test(tokio::test)]
async fn test_tool_list_notification() -> Result<()> {
    // Create a temporary directory for this test to avoid loading existing components
    let temp_dir = tempfile::tempdir()?;
    let plugin_dir_arg = format!("--plugin-dir={}", temp_dir.path().display());

    // Get the path to the built binary
    let binary_path = std::env::current_dir()
        .context("Failed to get current directory")?
        .join("target/debug/wassette");

    // Start the server with stdio transport (disable logs to avoid stdout pollution)
    let mut child = tokio::process::Command::new(&binary_path)
        .args(["serve", &plugin_dir_arg])
        .env("RUST_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start wassette with stdio transport")?;

    let stdin = child.stdin.take().context("Failed to get stdin handle")?;
    let stdout = child.stdout.take().context("Failed to get stdout handle")?;
    let stderr = child.stderr.take().context("Failed to get stderr handle")?;

    let mut stdin = stdin;
    let mut stdout = BufReader::new(stdout);
    let mut stderr = BufReader::new(stderr);

    // Give the server time to start
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Check if the process is still running
    if let Ok(Some(status)) = child.try_wait() {
        let mut stderr_output = String::new();
        let _ = stderr.read_line(&mut stderr_output).await;
        return Err(anyhow::anyhow!(
            "Server process exited with status: {:?}, stderr: {}",
            status,
            stderr_output
        ));
    }

    // Send MCP initialize request
    let initialize_request = r#"{"jsonrpc": "2.0", "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test-client", "version": "1.0.0"}}, "id": 1}
"#;

    stdin.write_all(initialize_request.as_bytes()).await?;
    stdin.flush().await?;

    // Read and verify initialize response
    let mut response_line = String::new();
    match tokio::time::timeout(
        Duration::from_secs(10),
        stdout.read_line(&mut response_line),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            return Err(anyhow::anyhow!("Failed to read initialize response: {}", e));
        }
        Err(_) => {
            let mut stderr_output = String::new();
            let _ =
                tokio::time::timeout(Duration::from_secs(1), stderr.read_line(&mut stderr_output))
                    .await;
            return Err(anyhow::anyhow!(
                "Timeout waiting for initialize response. Stderr: {}",
                stderr_output
            ));
        }
    }

    let response: serde_json::Value =
        serde_json::from_str(&response_line).context("Failed to parse initialize response")?;

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert!(response["result"].is_object());

    // Send initialized notification (required by MCP protocol)
    let initialized_notification = r#"{"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}
"#;

    stdin.write_all(initialized_notification.as_bytes()).await?;
    stdin.flush().await?;

    // Step 1: Send initial list_tools request to get baseline tool count
    let list_tools_request = r#"{"jsonrpc": "2.0", "method": "tools/list", "params": {}, "id": 2}
"#;

    stdin.write_all(list_tools_request.as_bytes()).await?;
    stdin.flush().await?;

    // Read initial tools list response
    let mut tools_response_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        stdout.read_line(&mut tools_response_line),
    )
    .await
    .context("Timeout waiting for initial tools/list response")?
    .context("Failed to read initial tools/list response")?;

    let initial_tools_response: serde_json::Value = serde_json::from_str(&tools_response_line)
        .context("Failed to parse initial tools/list response")?;

    assert_eq!(initial_tools_response["jsonrpc"], "2.0");
    assert_eq!(initial_tools_response["id"], 2);
    assert!(initial_tools_response["result"].is_object());
    assert!(initial_tools_response["result"]["tools"].is_array());

    let initial_tools = &initial_tools_response["result"]["tools"]
        .as_array()
        .unwrap();
    let initial_tool_count = initial_tools.len();
    println!("Initial tool count: {initial_tool_count}");

    // Build a component to load
    let component_path = build_fetch_component().await?;

    // Step 2: Load a component using the load-component tool
    let load_component_request = format!(
        r#"{{"jsonrpc": "2.0", "method": "tools/call", "params": {{"name": "load-component", "arguments": {{"path": "file://{}"}}}}, "id": 3}}
"#,
        component_path.to_str().unwrap()
    );

    stdin.write_all(load_component_request.as_bytes()).await?;
    stdin.flush().await?;

    // Read the tool list change notification first (this is what we're testing!)
    let mut notification_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(15),
        stdout.read_line(&mut notification_line),
    )
    .await
    .context("Timeout waiting for tool list change notification")?
    .context("Failed to read tool list change notification")?;

    let notification: serde_json::Value = serde_json::from_str(&notification_line)
        .context("Failed to parse tool list change notification")?;

    // Verify we received a tools/list_changed notification
    assert_eq!(notification["jsonrpc"], "2.0");
    assert_eq!(notification["method"], "notifications/tools/list_changed");
    println!("✓ Received tools/list_changed notification as expected");

    // Read the actual load-component response
    let mut load_response_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(15),
        stdout.read_line(&mut load_response_line),
    )
    .await
    .context("Timeout waiting for load-component response")?
    .context("Failed to read load-component response")?;

    let load_response: serde_json::Value = serde_json::from_str(&load_response_line)
        .context("Failed to parse load-component response")?;

    assert_eq!(load_response["jsonrpc"], "2.0");
    assert_eq!(load_response["id"], 3);

    // Check if the load succeeded
    if load_response["error"].is_object() {
        panic!("Failed to load component: {}", load_response["error"]);
    }
    assert!(load_response["result"].is_object());
    println!("✓ Component loaded successfully");

    // Step 3: Send another list_tools request to verify tools were added
    let list_tools_request_after = r#"{"jsonrpc": "2.0", "method": "tools/list", "params": {}, "id": 4}
"#;

    stdin.write_all(list_tools_request_after.as_bytes()).await?;
    stdin.flush().await?;

    // Read updated tools list response
    let mut updated_tools_response_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        stdout.read_line(&mut updated_tools_response_line),
    )
    .await
    .context("Timeout waiting for updated tools/list response")?
    .context("Failed to read updated tools/list response")?;

    let updated_tools_response: serde_json::Value =
        serde_json::from_str(&updated_tools_response_line)
            .context("Failed to parse updated tools/list response")?;

    assert_eq!(updated_tools_response["jsonrpc"], "2.0");
    assert_eq!(updated_tools_response["id"], 4);
    assert!(updated_tools_response["result"].is_object());
    assert!(updated_tools_response["result"]["tools"].is_array());

    let updated_tools = &updated_tools_response["result"]["tools"]
        .as_array()
        .unwrap();
    let updated_tool_count = updated_tools.len();
    println!("Updated tool count: {updated_tool_count}");

    // Verify that the tool count increased after loading the component
    assert!(
        updated_tool_count > initial_tool_count,
        "Tool count should have increased from {initial_tool_count} to {updated_tool_count}, but it didn't"
    );
    println!("✓ Tool count increased as expected after loading component");

    // Verify that the new tools from the component are present
    let updated_tool_names: Vec<String> = updated_tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or("").to_string())
        .collect();

    // The fetch component should add a "fetch" tool
    assert!(
        updated_tool_names.contains(&"fetch".to_string()),
        "Expected 'fetch' tool from loaded component, but found tools: {updated_tool_names:?}"
    );
    println!("✓ New tools from loaded component are present in the list");

    // Clean up
    child.kill().await.ok();

    Ok(())
}

#[test(tokio::test)]
async fn test_http_transport() -> Result<()> {
    // Use a random available port to avoid conflicts
    let port = find_open_port().await?;

    // We need to modify the source to support configurable bind address
    // For now, let's test with the default port but check if it's available
    let default_port = 9001u16;
    let test_port = if TcpListener::bind(format!("127.0.0.1:{default_port}"))
        .await
        .is_ok()
    {
        default_port
    } else {
        port
    };

    // If we're not using the default port, skip this test for now
    // since the server code uses a hardcoded bind address
    if test_port != default_port {
        println!("Skipping HTTP transport test: default port 9001 is not available");
        return Ok(());
    }

    // Create a temporary directory for this test to avoid loading existing components
    let temp_dir = tempfile::tempdir()?;
    let plugin_dir_arg = format!("--plugin-dir={}", temp_dir.path().display());

    // Get the path to the built binary
    let binary_path = std::env::current_dir()
        .context("Failed to get current directory")?
        .join("target/debug/wassette");

    // Start the server with HTTP transport
    let mut child = tokio::process::Command::new(&binary_path)
        .args(["serve", "--sse", &plugin_dir_arg])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start wassette with HTTP transport")?;

    // Give the server time to start (less time needed with empty plugin dir)
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Create HTTP client
    let client = reqwest::Client::new();
    let base_url = format!("http://127.0.0.1:{test_port}");

    // Test that the server is responding
    let response = tokio::time::timeout(Duration::from_secs(10), client.get(&base_url).send())
        .await
        .context("Timeout waiting for HTTP server response")?
        .context("Failed to connect to HTTP server")?;

    // The server should return some response (even if it's an error for GET requests)
    // The important thing is that it's listening and responding
    assert!(response.status().as_u16() >= 200);

    // Clean up
    child.kill().await.ok();

    Ok(())
}

#[test(tokio::test)]
async fn test_default_stdio_transport() -> Result<()> {
    // Create a temporary directory for this test to avoid loading existing components
    let temp_dir = tempfile::tempdir()?;
    let plugin_dir_arg = format!("--plugin-dir={}", temp_dir.path().display());

    // Get the path to the built binary
    let binary_path = std::env::current_dir()
        .context("Failed to get current directory")?
        .join("target/debug/wassette");

    // Start the server without any transport flags (should default to stdio)
    let mut child = tokio::process::Command::new(&binary_path)
        .args(["serve", &plugin_dir_arg])
        .env("RUST_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start wassette with default transport")?;

    let stdin = child.stdin.take().context("Failed to get stdin handle")?;
    let stdout = child.stdout.take().context("Failed to get stdout handle")?;

    let mut stdin = stdin;
    let mut stdout = BufReader::new(stdout);

    // Give the server time to start (less time needed with empty plugin dir)
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Check if the process is still running
    if let Ok(Some(status)) = child.try_wait() {
        return Err(anyhow::anyhow!(
            "Server process exited with status: {:?}",
            status
        ));
    }

    // Send MCP initialize request
    let initialize_request = r#"{"jsonrpc": "2.0", "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test-client", "version": "1.0.0"}}, "id": 1}
"#;

    stdin.write_all(initialize_request.as_bytes()).await?;
    stdin.flush().await?;

    // Read and verify response
    let mut response_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        stdout.read_line(&mut response_line),
    )
    .await
    .context("Timeout waiting for initialize response")?
    .context("Failed to read initialize response")?;

    let response: serde_json::Value =
        serde_json::from_str(&response_line).context("Failed to parse initialize response")?;

    // Verify the response structure (this confirms stdio transport is working)
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert!(response["result"].is_object());

    // Clean up
    child.kill().await.ok();

    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test(tokio::test)]
async fn test_grant_permission_network_basic() -> Result<()> {
    let (manager, _tempdir) = setup_lifecycle_manager().await?;
    let component_path = build_fetch_component().await?;

    let component_id = manager
        .load_component(&format!("file://{}", component_path.to_str().unwrap()))
        .await?
        .component_id;

    // Test granting network permission
    let result = manager
        .grant_permission(
            &component_id,
            "network",
            &serde_json::json!({"host": "api.example.com"}),
        )
        .await;

    assert!(result.is_ok());

    // Verify policy file was created and contains the permission
    let policy_info = manager.get_policy_info(&component_id).await;
    assert!(policy_info.is_some());
    let policy_info = policy_info.unwrap();

    // Verify policy contains the permission
    let policy_content = tokio::fs::read_to_string(&policy_info.local_path).await?;
    assert!(policy_content.contains("api.example.com"));
    assert!(policy_content.contains("network"));

    Ok(())
}

#[test(tokio::test)]
async fn test_disable_builtin_tools() -> Result<()> {
    // Create a temporary directory for this test to avoid loading existing components
    let temp_dir = tempfile::tempdir()?;
    let plugin_dir_arg = format!("--plugin-dir={}", temp_dir.path().display());

    // Get the path to the built binary
    let binary_path = std::env::current_dir()
        .context("Failed to get current directory")?
        .join("target/debug/wassette");

    // Start the server with stdio transport and disable-builtin-tools flag
    let mut child = tokio::process::Command::new(&binary_path)
        .args(["serve", &plugin_dir_arg, "--disable-builtin-tools"])
        .env("RUST_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start wassette with disabled builtin tools")?;

    let stdin = child.stdin.take().context("Failed to get stdin handle")?;
    let stdout = child.stdout.take().context("Failed to get stdout handle")?;
    let stderr = child.stderr.take().context("Failed to get stderr handle")?;

    let mut stdin = stdin;
    let mut stdout = BufReader::new(stdout);
    let mut stderr = BufReader::new(stderr);

    // Give the server time to start
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Check if the process is still running
    if let Ok(Some(status)) = child.try_wait() {
        let mut stderr_output = String::new();
        let _ = stderr.read_line(&mut stderr_output).await;
        return Err(anyhow::anyhow!(
            "Server process exited with status: {:?}, stderr: {}",
            status,
            stderr_output
        ));
    }

    // Send MCP initialize request
    let initialize_request = r#"{"jsonrpc": "2.0", "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test-client", "version": "1.0.0"}}, "id": 1}
"#;

    stdin.write_all(initialize_request.as_bytes()).await?;
    stdin.flush().await?;

    // Read and verify response
    let mut response_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        stdout.read_line(&mut response_line),
    )
    .await
    .context("Timeout waiting for initialize response")?
    .context("Failed to read initialize response")?;

    let response: serde_json::Value =
        serde_json::from_str(&response_line).context("Failed to parse initialize response")?;

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert!(response["result"].is_object());

    // Send initialized notification
    let initialized_notification = r#"{"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}
"#;

    stdin.write_all(initialized_notification.as_bytes()).await?;
    stdin.flush().await?;

    // Send list_tools request
    let list_tools_request = r#"{"jsonrpc": "2.0", "method": "tools/list", "params": {}, "id": 2}
"#;

    stdin.write_all(list_tools_request.as_bytes()).await?;
    stdin.flush().await?;

    // Read and verify tools list response
    let mut tools_response_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        stdout.read_line(&mut tools_response_line),
    )
    .await
    .context("Timeout waiting for tools/list response")?
    .context("Failed to read tools/list response")?;

    let tools_response: serde_json::Value = serde_json::from_str(&tools_response_line)
        .context("Failed to parse tools/list response")?;

    // Verify the tools response structure
    assert_eq!(tools_response["jsonrpc"], "2.0");
    assert_eq!(tools_response["id"], 2);
    assert!(tools_response["result"].is_object());
    assert!(tools_response["result"]["tools"].is_array());

    // Verify that built-in tools are NOT present when disabled
    let tools = &tools_response["result"]["tools"].as_array().unwrap();
    let tool_names: Vec<String> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or("").to_string())
        .collect();

    assert!(
        !tool_names.contains(&"load-component".to_string()),
        "load-component should not be present when builtin tools are disabled"
    );
    assert!(
        !tool_names.contains(&"unload-component".to_string()),
        "unload-component should not be present when builtin tools are disabled"
    );
    assert!(
        !tool_names.contains(&"list-components".to_string()),
        "list-components should not be present when builtin tools are disabled"
    );
    assert!(
        !tool_names.contains(&"get-policy".to_string()),
        "get-policy should not be present when builtin tools are disabled"
    );

    // Try to call a builtin tool and verify it fails
    let call_tool_request = r#"{"jsonrpc": "2.0", "method": "tools/call", "params": {"name": "list-components", "arguments": {}}, "id": 3}
"#;

    stdin.write_all(call_tool_request.as_bytes()).await?;
    stdin.flush().await?;

    // Read and verify call tool response
    let mut call_response_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        stdout.read_line(&mut call_response_line),
    )
    .await
    .context("Timeout waiting for tools/call response")?
    .context("Failed to read tools/call response")?;

    let call_response: serde_json::Value =
        serde_json::from_str(&call_response_line).context("Failed to parse tools/call response")?;

    // Verify that the tool call failed
    assert_eq!(call_response["jsonrpc"], "2.0");
    assert_eq!(call_response["id"], 3);
    assert!(call_response["result"].is_object());
    let result = &call_response["result"];
    assert_eq!(
        result["isError"].as_bool().unwrap_or(false),
        true,
        "Tool call should have failed"
    );

    // Clean up
    child.kill().await.ok();

    Ok(())
}
