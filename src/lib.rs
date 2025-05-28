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
    
        // Get the list of columns requested by the user
        let columns: Vec<String> = ctx.get_columns()
            .iter()
            .map(|col| col.name())
            .collect();

        // Helper to convert snake_case to camelCase
        fn snake_to_camel(s: &str) -> String {
            let mut result = String::new();
            let mut uppercase = false;
            for c in s.chars() {
                if c == '_' {
                    uppercase = true;
                } else if uppercase {
                    result.push(c.to_ascii_uppercase());
                    uppercase = false;
                } else {
                    result.push(c);
                }
            }
            result
        }

        // Convert all requested fields to camelCase for GraphQL and handle object fields
        let mut graphql_fields = Vec::new();
        for col in ctx.get_columns() {
            let col_name = col.name();
            // Handle special object fields that need subfield selection
            match col_name.as_str() {
                "state" => graphql_fields.push("state { id name color }".to_string()),
                "state_id" => graphql_fields.push("state { id }".to_string()),
                "team_id" => graphql_fields.push("team { id }".to_string()),
                "assignee_id" => graphql_fields.push("assignee { id }".to_string()),
                "creator_id" => graphql_fields.push("creator { id }".to_string()),
                "parent_id" => graphql_fields.push("parent { id }".to_string()),
                "project_id" => graphql_fields.push("project { id }".to_string()),
                "cycle_id" => graphql_fields.push("cycle { id }".to_string()),
                "labels" => graphql_fields.push("labels { nodes { id name color } }".to_string()),
                _ => graphql_fields.push(snake_to_camel(&col_name)),
            }
        }
        let fields = graphql_fields.join("\n          ");
        let mut query = String::new();
        let mut resp_pointer = String::new();
        
        // Process any WHERE clause conditions from quals
        let mut filter_conditions = String::new();
        let quals = ctx.get_quals();
        if !quals.is_empty() {
            let mut filters = Vec::new();
            for qual in quals {
                let field = snake_to_camel(&qual.field());
                let operator = qual.operator();
                let value = qual.value();
                
                // Map SQL operators to GraphQL filter operators based on Linear's schema
                // Linear uses different filter operators than standard GraphQL
                let filter_op = match operator.as_str() {
                    "=" => "eq",
                    "<>" => "neq",
                    ">" => "gt",
                    ">=" => "gte",
                    "<" => "lt",
                    "<=" => "lte",
                    "~~" => "contains", // LIKE in PostgreSQL
                    "!~~" => "notContains", // NOT LIKE in PostgreSQL
                    "~~*" => "containsIgnoreCase", // ILIKE in PostgreSQL
                    "!~~*" => "notContainsIgnoreCase", // NOT ILIKE in PostgreSQL
                    "IS NULL" => "null", 
                    "IS NOT NULL" => "notNull",
                    _ => continue, // Skip unsupported operators
                };
                
                // Format the value based on its type
                let formatted_value = match value {
                    bindings::supabase::wrappers::types::Value::Cell(cell) => {
                        match cell {
                            Cell::String(s) => format!("\"{}\"", s),
                            Cell::Bool(b) => b.to_string(),
                            Cell::I32(i) => i.to_string(),
                            Cell::I64(i) => i.to_string(),
                            Cell::Timestamp(ts) => format!("\"{}\"", ts),
                            _ => continue, // Skip unsupported types
                        }
                    },
                    _ => continue, // Skip if no value
                };
                
                filters.push(format!("{}: {{ {}: {} }}", field, filter_op, formatted_value));
            }
            
            if !filters.is_empty() {
                filter_conditions = format!("filter: {{ {} }}", filters.join(", "));
            }
        }

        // Build query and response pointer based on object/options
        match object.as_str() {
            "issues" => {
                // All issues with optional filter
                if filter_conditions.is_empty() {
                    query = format!(r#"{{ issues {{ nodes {{ {} }} }} }}"#, fields);
                } else {
                    query = format!(r#"{{ issues({}) {{ nodes {{ {} }} }} }}"#, filter_conditions, fields);
                }
                resp_pointer = "/data/issues/nodes".to_string();
            },
            "issue" => {
                // Specific issue by id
                let id = opts.get("id").ok_or("Missing required option 'id' for object 'issue'")?;
                query = format!(r#"{{ issue(id: \"{}\") {{ {} }} }}"#, id, fields);
                resp_pointer = "/data/issue".to_string();
            },
            "teams" => {
                // All teams with optional filter
                if filter_conditions.is_empty() {
                    query = format!(r#"{{ teams {{ nodes {{ {} }} }} }}"#, fields);
                } else {
                    query = format!(r#"{{ teams({}) {{ nodes {{ {} }} }} }}"#, filter_conditions, fields);
                }
                resp_pointer = "/data/teams/nodes".to_string();
            },
            "team" => {
                // Specific team by id
                let id = opts.get("id").ok_or("Missing required option 'id' for object 'team'")?;
                query = format!(r#"{{ team(id: \"{}\") {{ {} }} }}"#, id, fields);
                resp_pointer = "/data/team".to_string();
            },
            "projects" => {
                // All projects with optional filter
                if filter_conditions.is_empty() {
                    query = format!(r#"{{ projects {{ nodes {{ {} }} }} }}"#, fields);
                } else {
                    query = format!(r#"{{ projects({}) {{ nodes {{ {} }} }} }}"#, filter_conditions, fields);
                }
                resp_pointer = "/data/projects/nodes".to_string();
            },
            "project" => {
                // Specific project by id
                let id = opts.get("id").ok_or("Missing required option 'id' for object 'project'")?;
                query = format!(r#"{{ project(id: \"{}\") {{ {} }} }}"#, id, fields);
                resp_pointer = "/data/project".to_string();
            },
            "project_issues" => {
                // Issues within a project
                let project_id = opts.get("project_id").ok_or("Missing required option 'project_id' for object 'project_issues'")?;
                if filter_conditions.is_empty() {
                    query = format!(r#"{{ project(id: \"{}\") {{ issues {{ nodes {{ {} }} }} }} }}"#, project_id, fields);
                } else {
                    query = format!(r#"{{ project(id: \"{}\") {{ issues({}) {{ nodes {{ {} }} }} }} }}"#, project_id, filter_conditions, fields);
                }
                resp_pointer = "/data/project/issues/nodes".to_string();
            },
            "users" => {
                // All users with optional filter
                if filter_conditions.is_empty() {
                    query = format!(r#"{{ users {{ nodes {{ {} }} }} }}"#, fields);
                } else {
                    query = format!(r#"{{ users({}) {{ nodes {{ {} }} }} }}"#, filter_conditions, fields);
                }
                resp_pointer = "/data/users/nodes".to_string();
            },
            "user" => {
                // Specific user by id
                let id = opts.get("id").ok_or("Missing required option 'id' for object 'user'")?;
                query = format!(r#"{{ user(id: \"{}\") {{ {} }} }}"#, id, fields);
                resp_pointer = "/data/user".to_string();
            },
            "user_assigned_issues" => {
                // Issues assigned to a user
                let user_id = opts.get("user_id").ok_or("Missing required option 'user_id' for object 'user_assigned_issues'")?;
                if filter_conditions.is_empty() {
                    query = format!(r#"{{ user(id: \"{}\") {{ assignedIssues {{ nodes {{ {} }} }} }} }}"#, user_id, fields);
                } else {
                    query = format!(r#"{{ user(id: \"{}\") {{ assignedIssues({}) {{ nodes {{ {} }} }} }} }}"#, user_id, filter_conditions, fields);
                }
                resp_pointer = "/data/user/assignedIssues/nodes".to_string();
            },
            "cycles" => {
                // All cycles with optional filter
                if filter_conditions.is_empty() {
                    query = format!(r#"{{ cycles {{ nodes {{ {} }} }} }}"#, fields);
                } else {
                    query = format!(r#"{{ cycles({}) {{ nodes {{ {} }} }} }}"#, filter_conditions, fields);
                }
                resp_pointer = "/data/cycles/nodes".to_string();
            },
            "cycle_issues" => {
                // Issues in a cycle
                let cycle_id = opts.get("cycle_id").ok_or("Missing required option 'cycle_id' for object 'cycle_issues'")?;
                if filter_conditions.is_empty() {
                    query = format!(r#"{{ cycle(id: \"{}\") {{ issues {{ nodes {{ {} }} }} }} }}"#, cycle_id, fields);
                } else {
                    query = format!(r#"{{ cycle(id: \"{}\") {{ issues({}) {{ nodes {{ {} }} }} }} }}"#, cycle_id, filter_conditions, fields);
                }
                resp_pointer = "/data/cycle/issues/nodes".to_string();
            },
            "workflow_states" => {
                // All workflow states with optional filter
                if filter_conditions.is_empty() {
                    query = format!(r#"{{ workflowStates {{ nodes {{ {} }} }} }}"#, fields);
                } else {
                    query = format!(r#"{{ workflowStates({}) {{ nodes {{ {} }} }} }}"#, filter_conditions, fields);
                }
                resp_pointer = "/data/workflowStates/nodes".to_string();
            },
            "issue_labels" => {
                // All issue labels with optional filter
                if filter_conditions.is_empty() {
                    query = format!(r#"{{ issueLabels {{ nodes {{ {} }} }} }}"#, fields);
                } else {
                    query = format!(r#"{{ issueLabels({}) {{ nodes {{ {} }} }} }}"#, filter_conditions, fields);
                }
                resp_pointer = "/data/issueLabels/nodes".to_string();
            },
            _ => {
                return Err(format!("Unknown object type: {}", object));
            }
        }

        utils::report_info(&format!("GraphQL Query: {}", query));

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

        if resp.status_code != 200 {
            return Err(format!("Failed to get data: {}", resp.body));
        }
        
        // Check for GraphQL errors in the response
        let resp_json: JsonValue = serde_json::from_str(&resp.body)
            .map_err(|e| format!("Failed to parse JSON response: {}", e))?;
            
        if let Some(errors) = resp_json.get("errors") {
            if let Some(errors_array) = errors.as_array() {
                if !errors_array.is_empty() {
                    return Err(format!("GraphQL errors: {}", serde_json::to_string(errors).unwrap_or_default()));
                }
            }
        }

        // Always flatten to an array for iter_scan
        if let Some(arr) = resp_json.pointer(&resp_pointer) {
            if let Some(items) = arr.as_array() {
                this.src_rows = items.to_owned();
            } else if arr.is_object() {
                // For single object queries
                this.src_rows = vec![arr.to_owned()];
            } else {
                this.src_rows = vec![];
            }
        } else {
            this.src_rows = vec![];
        }

        utils::report_info(&format!("Got {} rows", this.src_rows.len()));
        Ok(())
    }

    fn iter_scan(ctx: &Context, row: &Row) -> Result<Option<u32>, FdwError> {
        let this = Self::this_mut();

        // Helper to convert snake_case to camelCase
        fn snake_to_camel(s: &str) -> String {
            let mut result = String::new();
            let mut uppercase = false;
            for c in s.chars() {
                if c == '_' {
                    uppercase = true;
                } else if uppercase {
                    result.push(c.to_ascii_uppercase());
                    uppercase = false;
                } else {
                    result.push(c);
                }
            }
            result
        }

        if this.src_idx >= this.src_rows.len() {
            return Ok(None);
        }

        let src_row = &this.src_rows[this.src_idx];
        for tgt_col in ctx.get_columns() {
            let tgt_col_name = tgt_col.name();
            
            // Handle special fields that are nested objects
            let cell = match tgt_col_name.as_str() {
                "state_id" => {
                    let state = src_row.as_object()
                        .and_then(|v| v.get("state"))
                        .and_then(|v| v.as_object())
                        .and_then(|v| v.get("id"))
                        .and_then(|v| v.as_str());
                    match state {
                        Some(id) => Some(Cell::String(id.to_owned())),
                        None => None,
                    }
                },
                "team_id" => {
                    let team = src_row.as_object()
                        .and_then(|v| v.get("team"))
                        .and_then(|v| v.as_object())
                        .and_then(|v| v.get("id"))
                        .and_then(|v| v.as_str());
                    match team {
                        Some(id) => Some(Cell::String(id.to_owned())),
                        None => None,
                    }
                },
                "assignee_id" => {
                    let assignee = src_row.as_object()
                        .and_then(|v| v.get("assignee"))
                        .and_then(|v| v.as_object())
                        .and_then(|v| v.get("id"))
                        .and_then(|v| v.as_str());
                    match assignee {
                        Some(id) => Some(Cell::String(id.to_owned())),
                        None => None,
                    }
                },
                "creator_id" => {
                    let creator = src_row.as_object()
                        .and_then(|v| v.get("creator"))
                        .and_then(|v| v.as_object())
                        .and_then(|v| v.get("id"))
                        .and_then(|v| v.as_str());
                    match creator {
                        Some(id) => Some(Cell::String(id.to_owned())),
                        None => None,
                    }
                },
                "parent_id" => {
                    let parent = src_row.as_object()
                        .and_then(|v| v.get("parent"))
                        .and_then(|v| v.as_object())
                        .and_then(|v| v.get("id"))
                        .and_then(|v| v.as_str());
                    match parent {
                        Some(id) => Some(Cell::String(id.to_owned())),
                        None => None,
                    }
                },
                "project_id" => {
                    let project = src_row.as_object()
                        .and_then(|v| v.get("project"))
                        .and_then(|v| v.as_object())
                        .and_then(|v| v.get("id"))
                        .and_then(|v| v.as_str());
                    match project {
                        Some(id) => Some(Cell::String(id.to_owned())),
                        None => None,
                    }
                },
                "cycle_id" => {
                    let cycle = src_row.as_object()
                        .and_then(|v| v.get("cycle"))
                        .and_then(|v| v.as_object())
                        .and_then(|v| v.get("id"))
                        .and_then(|v| v.as_str());
                    match cycle {
                        Some(id) => Some(Cell::String(id.to_owned())),
                        None => None,
                    }
                },
                "state" => {
                    let state = src_row.as_object()
                        .and_then(|v| v.get("state"))
                        .and_then(|v| v.as_object())
                        .and_then(|v| v.get("name"))
                        .and_then(|v| v.as_str());
                    match state {
                        Some(name) => Some(Cell::String(name.to_owned())),
                        None => None,
                    }
                },
                "labels" => {
                    // For labels, we'll return a JSON string representation
                    let labels = src_row.as_object()
                        .and_then(|v| v.get("labels"))
                        .and_then(|v| v.as_object())
                        .and_then(|v| v.get("nodes"))
                        .and_then(|v| v.as_array());
                    match labels {
                        Some(arr) => Some(Cell::Json(serde_json::to_string(arr).unwrap_or_default())),
                        None => None,
                    }
                },
                _ => {
                    // For regular fields, use the standard approach
                    let camel_name = snake_to_camel(&tgt_col_name);
                    let src = src_row
                        .as_object()
                        .and_then(|v| v.get(&camel_name));
                    
                    match src {
                        Some(value) => {
                            match tgt_col.type_oid() {
                                TypeOid::Bool => value.as_bool().map(Cell::Bool),
                                TypeOid::I32 => value.as_i64().map(|v| Cell::I32(v as i32)),
                                TypeOid::I64 => value.as_i64().map(Cell::I64),
                                TypeOid::String => value.as_str().map(|s| Cell::String(s.to_owned())),
                                TypeOid::Timestamp | TypeOid::Timestamptz => {
                                    if let Some(s) = value.as_str() {
                                        let ts = time::parse_from_rfc3339(s)?;
                                        Some(Cell::Timestamp(ts))
                                    } else {
                                        None
                                    }
                                }
                                TypeOid::Json => value.as_object().map(|_| Cell::Json(value.to_string())),
                                _ => {
                                    return Err(format!(
                                        "column {} data type is not supported",
                                        tgt_col_name
                                    ));
                                }
                            }
                        },
                        None => None,
                    }
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
        // All issues with extended fields
        format!(
            r#"-- GraphQL: {{ issues {{ nodes {{ ...fields }} }} }}
create foreign table if not exists issues (
id text,
title text,
description text,
number float,
priority float,
estimate float,
sub_issue_sort_order float,
priority_sort_order float,
state text,
state_id text,
team_id text,
assignee_id text,
creator_id text,
parent_id text,
project_id text,
cycle_id text,
created_at timestamptz,
updated_at timestamptz,
started_at timestamptz,
completed_at timestamptz,
archived_at timestamptz,
sort_order float,
due_date timestamptz,
url text
) server {} options (
object 'issues'
);"#,
            stmt.server_name,
        ),
        // A specific issue with extended fields
        format!(
            r#"-- GraphQL: {{ issue(id: $id) {{ ...fields }} }}
create foreign table if not exists issue (
id text,
title text,
description text,
number float,
priority float,
estimate float,
sub_issue_sort_order float,
priority_sort_order float,
state text,
state_id text,
team_id text,
assignee_id text,
creator_id text,
parent_id text,
project_id text,
cycle_id text,
created_at timestamptz,
updated_at timestamptz,
started_at timestamptz,
completed_at timestamptz,
archived_at timestamptz,
sort_order float,
due_date timestamptz,
url text
) server {} options (
object 'issue',
id 'YOUR_ISSUE_ID'
);"#,
            stmt.server_name,
        ),
        // All teams
        format!(
            r#"-- GraphQL: {{ teams {{ nodes {{ ...fields }} }} }}
create foreign table if not exists teams (
id text,
name text,
key text,
description text,
icon text,
color text,
cycles_enabled boolean,
cycle_start_day float,
cycle_duration float,
timezone text,
triage_enabled boolean,
private boolean,
created_at timestamptz,
updated_at timestamptz,
archived_at timestamptz
) server {} options (
object 'teams'
);"#,
            stmt.server_name,
        ),
        // All projects with extended fields
        format!(
            r#"-- GraphQL: {{ projects {{ nodes {{ ...fields }} }} }}
create foreign table if not exists projects (
id text,
name text,
description text,
icon text,
color text,
state text,
slug text,
team_id text,
creator_id text,
lead_id text,
sort_order float,
start_date timestamptz,
target_date timestamptz,
completed_at timestamptz,
created_at timestamptz,
updated_at timestamptz,
archived_at timestamptz,
url text
) server {} options (
object 'projects'
);"#,
            stmt.server_name,
        ),
        // Issues within a project
        format!(
            r#"-- GraphQL: {{ project(id: $project_id) {{ issues {{ nodes {{ ...fields }} }} }} }}
create foreign table if not exists project_issues (
id text,
title text,
description text,
number float,
priority float,
estimate float,
state text,
state_id text,
team_id text,
assignee_id text,
creator_id text,
project_id text,
created_at timestamptz,
updated_at timestamptz,
started_at timestamptz,
completed_at timestamptz,
archived_at timestamptz,
url text
) server {} options (
object 'project_issues',
project_id 'YOUR_PROJECT_ID'
);"#,
            stmt.server_name,
        ),
        // All users
        format!(
            r#"-- GraphQL: {{ users {{ nodes {{ ...fields }} }} }}
create foreign table if not exists users (
id text,
name text,
display_name text,
email text,
avatar_url text,
description text,
timezone text,
last_seen timestamptz,
active boolean,
url text,
created_at timestamptz,
updated_at timestamptz,
archived_at timestamptz
) server {} options (
object 'users'
);"#,
            stmt.server_name,
        ),
        // Issues assigned to a user
        format!(
            r#"-- GraphQL: {{ user(id: $user_id) {{ assignedIssues {{ nodes {{ ...fields }} }} }} }}
create foreign table if not exists user_assigned_issues (
id text,
title text,
description text,
number float,
priority float,
estimate float,
state text,
team_id text,
assignee_id text,
creator_id text,
project_id text,
created_at timestamptz,
updated_at timestamptz,
started_at timestamptz,
completed_at timestamptz,
archived_at timestamptz,
url text
) server {} options (
object 'user_assigned_issues',
user_id 'YOUR_USER_ID'
);"#,
            stmt.server_name,
        ),
        // All cycles
        format!(
            r#"-- GraphQL: {{ cycles {{ nodes {{ ...fields }} }} }}
create foreign table if not exists cycles (
id text,
number float,
name text,
description text,
start_date timestamptz,
end_date timestamptz,
completed_at timestamptz,
team_id text,
created_at timestamptz,
updated_at timestamptz,
archived_at timestamptz
) server {} options (
object 'cycles'
);"#,
            stmt.server_name,
        ),
        // Issues in a cycle
        format!(
            r#"-- GraphQL: {{ cycle(id: $cycle_id) {{ issues {{ nodes {{ ...fields }} }} }} }}
create foreign table if not exists cycle_issues (
id text,
title text,
description text,
number float,
priority float,
estimate float,
state text,
team_id text,
assignee_id text,
creator_id text,
project_id text,
cycle_id text,
created_at timestamptz,
updated_at timestamptz,
started_at timestamptz,
completed_at timestamptz,
archived_at timestamptz,
url text
) server {} options (
object 'cycle_issues',
cycle_id 'YOUR_CYCLE_ID'
);"#,
            stmt.server_name,
        ),
        // All workflow states
        format!(
            r#"-- GraphQL: {{ workflowStates {{ nodes {{ ...fields }} }} }}
create foreign table if not exists workflow_states (
id text,
name text,
description text,
color text,
type text,
position float,
team_id text,
created_at timestamptz,
updated_at timestamptz,
archived_at timestamptz
) server {} options (
object 'workflow_states'
);"#,
            stmt.server_name,
        ),
        // All issue labels
        format!(
            r#"-- GraphQL: {{ issueLabels {{ nodes {{ ...fields }} }} }}
create foreign table if not exists issue_labels (
id text,
name text,
description text,
color text,
team_id text,
created_at timestamptz,
updated_at timestamptz,
archived_at timestamptz
) server {} options (
object 'issue_labels'
);"#,
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