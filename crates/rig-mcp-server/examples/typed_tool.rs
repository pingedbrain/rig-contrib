//! A typed Rig tool served over MCP. The tool's arguments are a Rust struct
//! that derives `Deserialize` and `JsonSchema`; the schema the MCP client sees
//! is generated from that struct, so the two cannot disagree. No model and no
//! API key: the server is a tool only.
//!
//! The tool converts a temperature between Celsius, Fahrenheit and Kelvin.
//! Its `inputSchema` lists `value`, `from` and `to`, with `from` and `to`
//! limited to the three scales.
//!
//! Over stdio, the default:
//!
//! ```sh
//! cargo run -p rig-mcp-server --example typed_tool
//! ```
//!
//! Over Streamable HTTP at `/mcp`, on `PORT` (default 3400), with an optional
//! `MCP_TOKEN` bearer token:
//!
//! ```sh
//! PORT=3400 MCP_TOKEN=secret cargo run -p rig-mcp-server --example typed_tool -- http
//! ```
//!
//! To see the input schema, open the MCP Inspector and list the tools. Its
//! `convert_temperature` entry shows the schema, and calling it with
//! `{"value": 212, "from": "fahrenheit", "to": "celsius"}` returns `100`:
//!
//! ```sh
//! npx @modelcontextprotocol/inspector cargo run -p rig-mcp-server --example typed_tool
//! ```

use rig_core::completion::ToolAnnotations;
use rig_core::tool::{Tool, ToolContext, ToolExecutionError, args_schema};
use rig_mcp_server::McpServer;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A temperature scale.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Scale {
    Celsius,
    Fahrenheit,
    Kelvin,
}

/// The arguments of `convert_temperature`. The doc comments become the
/// property descriptions in the schema.
#[derive(Debug, Deserialize, JsonSchema)]
struct ConvertArgs {
    /// The temperature to convert.
    value: f64,
    /// The scale `value` is in.
    from: Scale,
    /// The scale to convert to.
    to: Scale,
}

/// The converted temperature, sent back as structured content.
#[derive(Debug, Serialize)]
struct Converted {
    value: f64,
    scale: Scale,
}

/// Converts a temperature between scales. Read-only, needs no model.
struct ConvertTemperature;

impl Tool for ConvertTemperature {
    const NAME: &'static str = "convert_temperature";
    type Args = ConvertArgs;
    type Output = Converted;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Convert a temperature between Celsius, Fahrenheit and Kelvin.".into()
    }

    fn parameters(&self) -> Value {
        args_schema::<ConvertArgs>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        }
    }

    async fn call(&self, _: &mut ToolContext, args: ConvertArgs) -> Result<Converted, Self::Error> {
        let kelvin = match args.from {
            Scale::Celsius => args.value + 273.15,
            Scale::Fahrenheit => (args.value - 32.0) * 5.0 / 9.0 + 273.15,
            Scale::Kelvin => args.value,
        };
        if kelvin < 0.0 {
            return Err(ToolExecutionError::other("below absolute zero"));
        }
        let value = match args.to {
            Scale::Celsius => kelvin - 273.15,
            Scale::Fahrenheit => (kelvin - 273.15) * 9.0 / 5.0 + 32.0,
            Scale::Kelvin => kelvin,
        };
        Ok(Converted {
            value,
            scale: args.to,
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), rig_mcp_server::ServeError> {
    let mut server = McpServer::new("typed-tools").tool(ConvertTemperature);

    if std::env::args().nth(1).as_deref() != Some("http") {
        return server.serve_stdio().await;
    }
    if let Ok(token) = std::env::var("MCP_TOKEN") {
        server = server.bearer_token(token);
    }
    let port: u16 = std::env::var("PORT").map_or(Ok(3400), |port| port.parse())?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    eprintln!("typed tools MCP server at http://127.0.0.1:{port}/mcp");
    rig_mcp_server::axum::serve(listener, server.into_router()?).await?;
    Ok(())
}
