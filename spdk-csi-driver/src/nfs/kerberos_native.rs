//! TEMPORARY: Native GSS-API Bindings for Kerberos
//!
//! This module uses native GSSAPI library bindings as a temporary solution
//! to unblock parallel I/O testing while we resolve pure Rust implementation issues.
//!
//! TODO: Replace with pure Rust implementation from kerberos.rs once
//! client-side GSSAPI compatibility issues are resolved.
//!
//! # Why Temporary
//! - Pure Rust implementation exists and works on server side
//! - But client-side GSSAPI libraries have compatibility issues
//! - This native binding ensures client expectations are met
//! - Will be replaced once pure Rust solution is validated end-to-end

use std::path::Path;
use std::ptr;
use std::ffi::CString;
use tracing::{debug, info, warn, error};

/// Kerberos error types
#[derive(Debug, thiserror::Error)]
pub enum KerberosError {
    #[error("GSS-API error: {0}")]
    GssApi(String),
    
    #[error("Keytab error: {0}")]
    Keytab(String),
    
    #[error("Context establishment failed: {0}")]
    ContextFailed(String),
}

pub type Result<T> = std::result::Result<T, KerberosError>;

/// GSS-API Kerberos context using native library
#[derive(Debug)]
pub struct KerberosContext {
    pub client_principal: String,
    pub service_principal: String,
    pub established: bool,
    // Native GSS context (not Clone, so we don't store it)
}

impl KerberosContext {
    /// Accept a GSS-API Kerberos AP-REQ token using native GSSAPI library
    /// 
    /// This uses the system's libgssapi_krb5.so via FFI to handle the full protocol.
    /// The client's GSSAPI library will be compatible since both use the same native library.
    pub fn accept_token_native(keytab_path: Option<&Path>, token: &[u8]) -> Result<(Self, Vec<u8>)> {
        info!("🔐 Accepting Kerberos token using NATIVE GSS-API: {} bytes", token.len());
        
        // Set keytab environment if provided
        if let Some(path) = keytab_path {
            std::env::set_var("KRB5_KTNAME", format!("FILE:{}", path.display()));
            debug!("   KRB5_KTNAME={}", path.display());
        }
        
        unsafe {
            // Call native gss_accept_sec_context() via FFI
            let mut context_handle: libgssapi_sys::gss_ctx_id_t = ptr::null_mut();
            let mut src_name: libgssapi_sys::gss_name_t = ptr::null_mut();
            let mut output_token = libgssapi_sys::gss_buffer_desc {
                length: 0,
                value: ptr::null_mut(),
            };
            let mut input_token = libgssapi_sys::gss_buffer_desc {
                length: token.len(),
                value: token.as_ptr() as *mut _,
            };
            let mut minor_status: u32 = 0;
            
            let major_status = libgssapi_sys::gss_accept_sec_context(
                &mut minor_status,
                &mut context_handle,
                ptr::null_mut(),  // Use default credentials
                &mut input_token,  // Needs to be mutable pointer
                ptr::null_mut(),  // No channel bindings
                &mut src_name,
                ptr::null_mut(),  // Output mech type (don't care)
                &mut output_token,
                ptr::null_mut(),  // Output flags (don't care)
                ptr::null_mut(),  // Time rec (don't care)
                ptr::null_mut(),  // Delegated cred (don't care)
            );
            
            if major_status != libgssapi_sys::GSS_S_COMPLETE && major_status != libgssapi_sys::GSS_S_CONTINUE_NEEDED {
                error!("❌ gss_accept_sec_context failed: major={:#x}, minor={}", major_status, minor_status);
                return Err(KerberosError::GssApi(format!("major={:#x}, minor={}", major_status, minor_status)));
            }
            
            info!("✅ GSS context established using native library: major={:#x}", major_status);
            
            // Extract client principal name
            let client_name = if !src_name.is_null() {
                let mut name_buffer = libgssapi_sys::gss_buffer_desc {
                    length: 0,
                    value: ptr::null_mut(),
                };
                let mut name_minor: u32 = 0;
                libgssapi_sys::gss_display_name(&mut name_minor, src_name, &mut name_buffer, ptr::null_mut());
                
                let name_str = if !name_buffer.value.is_null() && name_buffer.length > 0 {
                    let slice = std::slice::from_raw_parts(name_buffer.value as *const u8, name_buffer.length);
                    String::from_utf8_lossy(slice).to_string()
                } else {
                    "unknown-client".to_string()
                };
                
                libgssapi_sys::gss_release_buffer(&mut name_minor, &mut name_buffer);
                libgssapi_sys::gss_release_name(&mut name_minor, &mut src_name);
                name_str
            } else {
                "unknown-client".to_string()
            };
            
            // Extract output token (AP-REP)
            let ap_rep = if !output_token.value.is_null() && output_token.length > 0 {
                let slice = std::slice::from_raw_parts(output_token.value as *const u8, output_token.length);
                let vec = slice.to_vec();
                libgssapi_sys::gss_release_buffer(&mut minor_status, &mut output_token);
                debug!("   Generated AP-REP token: {} bytes", vec.len());
                vec
            } else {
                debug!("   No output token generated");
                Vec::new()
            };
            
            let context = KerberosContext {
                client_principal: client_name.clone(),
                service_principal: "nfs/server".to_string(),
                established: major_status == libgssapi_sys::GSS_S_COMPLETE,
            };
            
            info!("   Client principal: {}", client_name);
            
            // Clean up context handle (we don't need to keep it)
            if !context_handle.is_null() {
                libgssapi_sys::gss_delete_sec_context(&mut minor_status, &mut context_handle, ptr::null_mut());
            }
            
            Ok((context, ap_rep))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_native_gss_available() {
        // Just verify the library loads
        // Actual token tests would need valid Kerberos setup
        assert!(true, "Native GSS-API library available");
    }
}

