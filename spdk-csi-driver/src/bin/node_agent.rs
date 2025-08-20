// bin/node_agent.rs - Node Agent Binary Entry Point
//
// This is the main binary entry point for the SPDK node agent.
// The actual implementation is in the spdk_csi_driver::node_agent module.

use spdk_csi_driver::node_agent::{NodeAgent, start_api_server, run_discovery_loop};
use std::env;
use kube::Client;
use clap::{Arg, Command};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Parse command line arguments
    let matches = Command::new("node-agent")
        .version("1.0")
        .about("SPDK CSI Node Agent")
        .arg(
            Arg::new("validate-only")
                .long("validate-only")
                .action(clap::ArgAction::SetTrue)
                .help("Run validation checks only (for init container use)")
        )
        .get_matches();
    
    // Check if running in validation-only mode
    let validate_only = matches.get_flag("validate-only");
    
    // Initialize tracing
    tracing_subscriber::fmt::init();
    
    if validate_only {
        println!("🧪 [INIT_VALIDATION] Running userspace SPDK validation checks...");
    } else {
        println!("🚀 [NODE_AGENT] Starting SPDK Node Agent...");
    }
    
    // Get configuration from environment
    let node_name = env::var("NODE_NAME").unwrap_or_else(|_| "unknown-node".to_string());
    let discovery_interval = env::var("DISCOVERY_INTERVAL")
        .unwrap_or_else(|_| "60".to_string())
        .parse()
        .unwrap_or(60);
    let auto_initialize_blobstore = env::var("AUTO_INITIALIZE_BLOBSTORE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    let backup_path = env::var("BACKUP_PATH").unwrap_or_else(|_| "/tmp/spdk-backup".to_string());
    let spdk_rpc_url = env::var("SPDK_RPC_URL").unwrap_or_else(|_| "http://localhost:9998".to_string());
    let target_namespace = env::var("TARGET_NAMESPACE").unwrap_or_else(|_| "default".to_string());
    let cluster_id = env::var("CLUSTER_ID").unwrap_or_else(|_| "flint-cluster".to_string());
    
    println!("📋 [NODE_AGENT] Configuration:");
    println!("  Node Name: {}", node_name);
    println!("  SPDK RPC URL: {}", spdk_rpc_url);
    println!("  Discovery Interval: {}s", discovery_interval);
    println!("  Auto Initialize Blobstore: {}", auto_initialize_blobstore);
    println!("  Target Namespace: {}", target_namespace);
    
    // Initialize Kubernetes client
    let kube_client = Client::try_default().await?;
    
    // Create the node agent
    let agent = NodeAgent::new(
        node_name,
        kube_client,
        spdk_rpc_url,
        discovery_interval,
        auto_initialize_blobstore,
        backup_path,
        target_namespace,
        cluster_id,
    );
    
    // Handle validation-only mode (for init container)
    if validate_only {
        println!("🧪 [INIT_VALIDATION] Performing userspace SPDK validation checks...");
        
        match agent.validate_driver_environment().await {
            Ok(_) => {
                println!("✅ [INIT_VALIDATION] Validation PASSED - environment supports userspace SPDK");
                println!("✅ [INIT_VALIDATION] Init container validation successful");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("❌ [INIT_VALIDATION] Validation FAILED: {}", e);
                eprintln!("❌ [INIT_VALIDATION] This node does not support userspace SPDK operations");
                eprintln!("💡 [INIT_VALIDATION] Required: 1) Kernel driver unbinding capability");
                eprintln!("💡 [INIT_VALIDATION] Required: 2) Userspace drivers (vfio-pci or uio_pci_generic)");
                eprintln!("💡 [INIT_VALIDATION] Required: 3) Write access to /sys/bus/pci/drivers_probe");
                eprintln!("🚫 [INIT_VALIDATION] SPDK containers will NOT start on this node");
                std::process::exit(1);
            }
        }
    }
    
    // Normal mode - perform validation but continue on failure (with warnings)
    println!("🧪 [NODE_AGENT] Performing startup validation...");
    if let Err(e) = agent.validate_driver_environment().await {
        eprintln!("❌ [NODE_AGENT] Startup validation failed: {}", e);
        eprintln!("💡 [NODE_AGENT] This environment may not support userspace SPDK operations");
        eprintln!("📊 [NODE_AGENT] The pod will continue to start but storage operations may fail");
        // Note: We continue starting but with warnings - this allows the pod to run
        // and provide diagnostic information via its API
    } else {
        println!("✅ [NODE_AGENT] Startup validation passed - environment supports userspace SPDK");
    }
    
    // Wait for SPDK to be ready before configuring
    println!("⏳ [NODE_AGENT] Waiting for SPDK to be ready...");
    if let Err(e) = spdk_csi_driver::node_agent::wait_for_spdk_ready(&agent).await {
        eprintln!("❌ [NODE_AGENT] SPDK failed to become ready: {}", e);
        return Err(e);
    }
    
    // Configure SPDK storage
    println!("🔧 [NODE_AGENT] Configuring SPDK storage...");
    let config_loaded = agent.configure_spdk_from_configmap(&agent.target_namespace).await?;
    
    if !config_loaded {
        println!("🔍 [NODE_AGENT] No ConfigMap found, performing auto-discovery...");
        if let Err(e) = agent.auto_configure_spdk_storage().await {
            eprintln!("⚠️ [NODE_AGENT] Auto-configuration failed: {}", e);
            eprintln!("💡 [NODE_AGENT] Storage may not be available, but continuing startup...");
        }
    }
    
    println!("🎯 [NODE_AGENT] Starting API server and discovery loop...");
    
    // Start API server and discovery loop concurrently
    let api_task = tokio::spawn(start_api_server(agent.clone()));
    let discovery_task = tokio::spawn(run_discovery_loop(agent.clone()));
    
    // Wait for either task to complete (or both)
    tokio::select! {
        result = api_task => {
            match result {
                Ok(_) => println!("✅ [NODE_AGENT] API server completed"),
                Err(e) => eprintln!("❌ [NODE_AGENT] API server failed: {}", e),
            }
        }
        result = discovery_task => {
            match result {
                Ok(_) => println!("✅ [NODE_AGENT] Discovery loop completed"),
                Err(e) => eprintln!("❌ [NODE_AGENT] Discovery loop failed: {}", e),
            }
        }
    }
    
    println!("🛑 [NODE_AGENT] Node agent shutting down");
    Ok(())
}
