mod auth;
mod error;
mod http;
mod protocol;
mod repo;
mod search;
mod types;

pub use auth::{
    check_auth, fetch_jwt, get_api_key, get_cached_jwt, get_config_path, get_jwt_exp,
    load_cached_api_key, save_cached_api_key,
};
pub use error::FastContextError;
pub use http::decode_unary_response;
pub use protocol::{build_system_prompt, get_tool_definitions};
pub use repo::{RepoMap, get_repo_map, parse_answer};
pub use search::{search, search_with_content, search_with_streaming};
pub use types::{AuthCheck, FileEntry, SearchMeta, SearchOptions, SearchResult};

pub const API_BASE: &str =
    "https://server.self-serve.windsurf.com/exa.api_server_pb.ApiServerService";
pub const AUTH_BASE: &str = "https://server.self-serve.windsurf.com/exa.auth_pb.AuthService";
pub const WS_APP: &str = "windsurf";
pub const DEFAULT_WS_APP_VER: &str = "1.48.2";
pub const DEFAULT_WS_LS_VER: &str = "1.9544.35";
pub const MAX_TREE_BYTES: usize = 32 * 1024;
