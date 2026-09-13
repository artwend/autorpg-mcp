//! Helpers for building MCP `ErrorData` values.

use rmcp::model::ErrorCode;
use rmcp::ErrorData as McpError;

/// Wraps an input-simulation failure as an internal MCP error.
pub fn input_error(message: impl std::fmt::Display) -> McpError {
    McpError::new(
        ErrorCode::INTERNAL_ERROR,
        format!("Input simulation failure: {}", message),
        None,
    )
}

/// Wraps an invalid tool argument as an invalid-params MCP error.
pub fn invalid_params(message: impl std::fmt::Display) -> McpError {
    McpError::new(ErrorCode::INVALID_PARAMS, message.to_string(), None)
}

/// Wraps an internal (serialization/IO) failure as an internal MCP error.
pub fn internal_error(message: impl std::fmt::Display) -> McpError {
    McpError::new(ErrorCode::INTERNAL_ERROR, message.to_string(), None)
}
