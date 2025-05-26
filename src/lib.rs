#[allow(warnings)]
mod bindings;
use serde_json::Value as JsonValue;

use bindings::{
    exports::supabase::wrappers::routines::Guest,
    supabase::wrappers::{
        http, time,
        types::{Cell, Context, FdwError, FdwResult, ImportForeignSchemaStmt, OptionsType, Row, TypeOid},
        utils,
    },
};

#[derive(Debug, Default)]
struct LinearFdw {
    base_url: String,
    src_rows: Vec<JsonValue>,
    src_idx: usize,
    api_key: String,
}

// pointer for the static FDW instance
static mut INSTANCE: *mut LinearFdw = std::ptr::null_mut::<LinearFdw>();

impl LinearFdw {
    // initialise FDW instance
    fn init_instance() {
        let instance = Self::default();
        unsafe {
            INSTANCE = Box::leak(Box::new(instance));
        }
    }

    fn this_mut() -> &'static mut Self {
        unsafe { &mut (*INSTANCE) }
    }
}

impl Guest for LinearFdw {
    fn host_version_requirement() -> String {
        // semver expression for Wasm FDW host version requirement
        // ref: https://docs.rs/semver/latest/semver/enum.Op.html
        "^0.1.0".to_string()
    }

    fn init(ctx: &Context) -> FdwResult {
        Self::init_instance();
        let this = Self::this_mut();

        let opts = ctx.get_options(&OptionsType::Server);
        this.base_url = opts.require_or("api_url", "https://api.linear.app/graphql");
        this.api_key = match opts.get("api_key") {
            Some(key) => key,
            None => {
                let key_id = opts.require("api_key_id")?;
                utils::get_vault_secret(&key_id).unwrap_or_default()
            }
        };

        Ok(())
    }

    fn begin_scan(ctx: &Context) -> FdwResult {
        let this = Self::this_mut();
    
        let opts = ctx.get_options(&OptionsType::Table);
        let object = opts.require("object")?;
        let url = this.base_url.clone();
    
        let query = format!(r#"
        {{
          {} {{
            nodes {{
              id
              title
            }}
          }}
        }}"#, object);
    
        let body = serde_json::json!({
            "query": query
        }).to_string();
    
        let headers = vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("authorization".to_owned(), this.api_key.to_owned()),
        ];
    
        let req = http::Request {
            method: http::Method::Post,
            url,
            headers,
            body,
        };
    
        let resp = http::post(&req)?;

        utils::report_info(&format!("Response: {}", resp.body));

        if resp.status_code != 200 {
            return Err(format!("Failed to get issues: {}", resp.body));
        }
        let resp_json: JsonValue = serde_json::from_str(&resp.body)
            .map_err(|e| format!("Failed to parse JSON response: {}", e))?;
    
        if let Some(issues) = resp_json.pointer(&format!("/data/{}/nodes", object)) {
            this.src_rows = issues.as_array()
                .map(|v| v.to_owned())
                .unwrap_or_default();
        }
    
        utils::report_info(&format!("Got {} issues", this.src_rows.len()));
    
        Ok(())
    }

    fn iter_scan(ctx: &Context, row: &Row) -> Result<Option<u32>, FdwError> {
        let this = Self::this_mut();

        if this.src_idx >= this.src_rows.len() {
            return Ok(None);
        }

        let src_row = &this.src_rows[this.src_idx];
        for tgt_col in ctx.get_columns() {
            let tgt_col_name = tgt_col.name();
            let src = src_row
                .as_object()
                .and_then(|v| v.get(&tgt_col_name))
                .ok_or(format!("source column '{}' not found", tgt_col_name))?;
            let cell = match tgt_col.type_oid() {
                TypeOid::Bool => src.as_bool().map(Cell::Bool),
                TypeOid::String => src.as_str().map(|v| Cell::String(v.to_owned())),
                TypeOid::Timestamp => {
                    if let Some(s) = src.as_str() {
                        let ts = time::parse_from_rfc3339(s)?;
                        Some(Cell::Timestamp(ts))
                    } else {
                        None
                    }
                }
                TypeOid::Json => src.as_object().map(|_| Cell::Json(src.to_string())),
                _ => {
                    return Err(format!(
                        "column {} data type is not supported",
                        tgt_col_name
                    ));
                }
            };

            row.push(cell.as_ref());
        }

        this.src_idx += 1;

        Ok(Some(0))
    }
    fn import_foreign_schema(
        _ctx: &Context,
        stmt: ImportForeignSchemaStmt,
    ) -> Result<Vec<String>, FdwError> {
        let ret = vec![
            format!(
                r#"create foreign table if not exists my_profile (
                    id bigint,
                    username text,
                    email text,
                    attrs jsonb
                )
                server {} options (
                    object 'my_profile'
                )"#,
                stmt.server_name,
            ),
            format!(
                r#"create foreign table if not exists event_types (
                    attrs jsonb
                )
                server {} options (
                    object 'event-types'
                )"#,
                stmt.server_name,
            ),
            format!(
                r#"create foreign table if not exists bookings (
                    attrs jsonb
                )
                server {} options (
                    object 'bookings',
                    rowid_column 'attrs'
                )"#,
                stmt.server_name,
            ),
            format!(
                r#"create foreign table if not exists calendars (
                    attrs jsonb
                )
                server {} options (
                    object 'calendars'
                )"#,
                stmt.server_name,
            ),
            format!(
                r#"create foreign table if not exists schedules (
                    id bigint,
                    name text,
                    attrs jsonb
                )
                server {} options (
                    object 'schedules'
                )"#,
                stmt.server_name,
            ),
            format!(
                r#"create foreign table if not exists conferencing (
                    id bigint,
                    attrs jsonb
                )
                server {} options (
                    object 'conferencing'
                )"#,
                stmt.server_name,
            ),
        ];
        Ok(ret)
    }

    fn re_scan(_ctx: &Context) -> FdwResult {
        Err("re_scan on foreign table is not supported".to_owned())
    }

    fn end_scan(_ctx: &Context) -> FdwResult {
        let this = Self::this_mut();
        this.src_rows.clear();
        Ok(())
    }

    fn begin_modify(_ctx: &Context) -> FdwResult {
        Err("modify on foreign table is not supported".to_owned())
    }

    fn insert(_ctx: &Context, _row: &Row) -> FdwResult {
        Ok(())
    }

    fn update(_ctx: &Context, _rowid: Cell, _row: &Row) -> FdwResult {
        Ok(())
    }

    fn delete(_ctx: &Context, _rowid: Cell) -> FdwResult {
        Ok(())
    }

    fn end_modify(_ctx: &Context) -> FdwResult {
        Ok(())
    }
}

bindings::export!(LinearFdw with_types_in bindings);