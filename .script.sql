
DROP FOREIGN DATA WRAPPER wasm_wrapper CASCADE;
DROP SCHEMA linear;

CREATE EXTENSION IF NOT EXISTS wrappers WITH SCHEMA extensions;

CREATE FOREIGN DATA WRAPPER wasm_wrapper
  HANDLER wasm_fdw_handler
  VALIDATOR wasm_fdw_validator;

CREATE SERVER linear_server
FOREIGN DATA WRAPPER wasm_wrapper
  OPTIONS (
    fdw_package_url 'file:///linear_fdw.wasm',
    fdw_package_name 'supabase:linear-fdw',
    fdw_package_version '0.1.0',
    api_url 'https://api.linear.app/graphql',
    api_key ''
  );

CREATE FOREIGN TABLE linear_issues (
  id text,
  title text
)
SERVER linear_server
OPTIONS (
  object 'issues'
);

CREATE SCHEMA linear;
IMPORT FOREIGN SCHEMA linear FROM SERVER linear_server INTO linear;

SELECT * FROM linear_issues;
