// build.rs -- Parse Wine's protocol.def and server_protocol.h to generate Rust
// protocol types at compile time.
//
// Generates from protocol.def (the canonical source of truth):
//   1. RequestCode enum + from_i32() + as_str()        (opcode values)
//   2. #[repr(C)] request/reply structs for all 306 requests (with padding)
//   3. RequestHandler trait with default STATUS_NOT_IMPLEMENTED stubs
//   4. dispatch_request() function routing all opcodes
//
// Generates from server_protocol.h (secondary):
//   5. enum message_type -> MSG_* constants
//   6. C_ASSERT(sizeof(struct X) == N) -> compile-time size cross-checks
//
// Wine source resolution order:
//   1. WINE_SRC env var
//   2. ~/.local/share/amphetamine/wine-src/
//   3. /tmp/proton-wine/
//
// If no Wine source found, uses src/protocol_generated.rs.fallback.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

// ── Type mapping table ───────────────────────────────────────────────────────
// Replicates Wine's tools/make_requests %formats hash.
// (rust_type, size, alignment)

fn type_map() -> HashMap<&'static str, (&'static str, usize, usize)> {
    let mut m = HashMap::new();
    // Primitives
    m.insert("int", ("i32", 4, 4));
    m.insert("short int", ("i16", 2, 2));
    m.insert("char", ("u8", 1, 1));
    m.insert("unsigned char", ("u8", 1, 1));
    m.insert("unsigned short", ("u16", 2, 2));
    m.insert("unsigned int", ("u32", 4, 4));
    m.insert("unsigned __int64", ("u64", 8, 8));
    // Wine typedefs
    m.insert("data_size_t", ("u32", 4, 4));
    m.insert("obj_handle_t", ("u32", 4, 4));
    m.insert("atom_t", ("u32", 4, 4));
    m.insert("process_id_t", ("u32", 4, 4));
    m.insert("thread_id_t", ("u32", 4, 4));
    m.insert("ioctl_code_t", ("u32", 4, 4));
    m.insert("user_handle_t", ("u32", 4, 4));
    m.insert("timeout_t", ("i64", 8, 8));
    m.insert("abstime_t", ("i64", 8, 8));
    m.insert("lparam_t", ("u64", 8, 8));
    m.insert("apc_param_t", ("u64", 8, 8));
    m.insert("mem_size_t", ("u64", 8, 8));
    m.insert("file_pos_t", ("u64", 8, 8));
    m.insert("client_ptr_t", ("u64", 8, 8));
    m.insert("affinity_t", ("u64", 8, 8));
    m.insert("mod_handle_t", ("u64", 8, 8));
    m.insert("object_id_t", ("u64", 8, 8));
    // Compound types -- opaque byte arrays, just need correct size/align
    m.insert("union apc_call", ("[u8; 64]", 64, 8));
    m.insert("union apc_result", ("[u8; 40]", 40, 8));
    m.insert("struct async_data", ("[u8; 40]", 40, 8));
    m.insert("struct context_data", ("[u8; 1720]", 1720, 8));
    m.insert("struct cursor_pos", ("[u8; 24]", 24, 8));
    m.insert("union debug_event_data", ("[u8; 160]", 160, 8));
    m.insert("struct filesystem_event", ("[u8; 12]", 12, 4));
    m.insert("struct generic_map", ("[u8; 16]", 16, 4));
    m.insert("struct handle_info", ("[u8; 32]", 32, 8));
    m.insert("union hw_input", ("[u8; 40]", 40, 8));
    m.insert("union irp_params", ("[u8; 32]", 32, 8));
    m.insert("struct luid", ("[u8; 8]", 8, 4));
    m.insert("struct luid_attr", ("[u8; 12]", 12, 4));
    m.insert("union message_data", ("[u8; 48]", 48, 8));
    m.insert("struct object_attributes", ("[u8; 16]", 16, 4));
    m.insert("struct object_type_info", ("[u8; 44]", 44, 4));
    m.insert("struct obj_locator", ("[u8; 16]", 16, 8));
    m.insert("struct pe_image_info", ("[u8; 88]", 88, 8));
    m.insert("struct process_info", ("[u8; 40]", 40, 8));
    m.insert("struct property_data", ("[u8; 16]", 16, 8));
    m.insert("struct rawinput_device", ("[u8; 12]", 12, 4));
    m.insert("struct rectangle", ("[u8; 16]", 16, 4));
    m.insert("union select_op", ("[u8; 264]", 264, 8));
    m.insert("struct startup_info_data", ("[u8; 96]", 96, 4));
    m.insert("union tcp_connection", ("[u8; 60]", 60, 4));
    m.insert("struct thread_info", ("[u8; 40]", 40, 8));
    m.insert("union udp_endpoint", ("[u8; 32]", 32, 4));
    m.insert("struct user_apc", ("[u8; 40]", 40, 8));
    // D3DKMT handles (upstream Wine 11+)
    m.insert("d3dkmt_handle_t", ("u32", 4, 4));
    // Shared memory types (Proton-specific)
    m.insert("desktop_shm_t", ("u32", 4, 4));
    m.insert("queue_shm_t", ("u32", 4, 4));
    m.insert("input_shm_t", ("u32", 4, 4));
    m.insert("object_shm_t", ("u32", 4, 4));
    m
}

// ── Data structures ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Field {
    name: String,
    rust_type: String,
    size: usize,
    align: usize,
}

#[derive(Debug, Clone)]
struct RequestDef {
    name: String,             // e.g. "close_handle"
    index: i32,               // opcode value
    req_fields: Vec<Field>,   // fixed-size request fields (excludes header)
    reply_fields: Vec<Field>, // fixed-size reply fields (excludes header)
    has_reply: bool,          // whether @REPLY section exists
    req_varargs: Vec<String>, // VARARG comments for request
    reply_varargs: Vec<String>, // VARARG comments for reply
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();
    let out_path = PathBuf::from(&out_dir).join("protocol_generated.rs");
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let fallback_path = PathBuf::from(&manifest_dir)
        .join("src")
        .join("protocol_generated.rs.fallback");

    let wine_src = find_wine_src();

    match wine_src {
        Some(wine_root) => {
            println!(
                "cargo:warning=Generating protocol from {}",
                wine_root.display()
            );

            let generated = generate_from_wine_src(&wine_root);
            fs::write(&out_path, &generated).expect("Failed to write generated protocol");

            // Update fallback for future builds without Wine source
            if let Err(e) = fs::write(&fallback_path, &generated) {
                println!("cargo:warning=Cannot update fallback file: {e}");
            }

            // Re-run if sources change
            let protocol_def = wine_root.join("server").join("protocol.def");
            let header = wine_root
                .join("include")
                .join("wine")
                .join("server_protocol.h");
            if protocol_def.exists() {
                println!("cargo:rerun-if-changed={}", protocol_def.display());
            }
            if header.exists() {
                println!("cargo:rerun-if-changed={}", header.display());
            }
        }
        None => {
            if fallback_path.exists() {
                println!("cargo:warning=Wine source not found, using committed fallback");
                fs::copy(&fallback_path, &out_path).expect("Failed to copy fallback");
            } else {
                panic!(
                    "No Wine source found and no fallback at {}. \
                     Set WINE_SRC or run: triskelion clone",
                    fallback_path.display()
                );
            }
        }
    }

    // Re-run if these change
    println!("cargo:rerun-if-env-changed=WINE_SRC");
    println!("cargo:rerun-if-changed=src/protocol_generated.rs.fallback");
}

/// Find the Wine source root directory.
fn find_wine_src() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = vec![
        env::var("WINE_SRC").ok().map(PathBuf::from).unwrap_or_default(),
        home_dir()
            .map(|h| {
                h.join(".local")
                    .join("share")
                    .join("amphetamine")
                    .join("wine-src")
            })
            .unwrap_or_default(),
        PathBuf::from("/tmp/proton-wine"),
    ];

    candidates.into_iter().find(|p| {
        p.join("server").join("protocol.def").exists()
            || p.join("include")
                .join("wine")
                .join("server_protocol.h")
                .exists()
    })
}

fn home_dir() -> Option<PathBuf> {
    env::var("HOME").ok().map(PathBuf::from)
}

// ── Top-level generation ─────────────────────────────────────────────────────

fn generate_from_wine_src(wine_root: &Path) -> String {
    let mut out = String::with_capacity(128 * 1024);

    let protocol_def = wine_root.join("server").join("protocol.def");
    let header_path = wine_root
        .join("include")
        .join("wine")
        .join("server_protocol.h");

    out.push_str(&format!(
        "// Auto-generated from Wine protocol sources -- do not edit manually.\n\
         // Wine root: {}\n\
         // Generated by build.rs at compile time.\n\n",
        wine_root.display()
    ));

    let types = type_map();

    // Parse protocol.def for full request definitions
    let requests = if protocol_def.exists() {
        let def_content = fs::read_to_string(&protocol_def).unwrap_or_else(|e| {
            panic!("Failed to read {}: {e}", protocol_def.display());
        });
        let reqs = parse_protocol_def_full(&def_content, &types);
        println!(
            "cargo:warning=  protocol.def: {} request opcodes",
            reqs.len()
        );
        reqs
    } else if header_path.exists() {
        // Fallback: parse server_protocol.h for just the names (no field info)
        println!("cargo:warning=  protocol.def not found, falling back to server_protocol.h");
        let content = fs::read_to_string(&header_path).unwrap_or_else(|e| {
            panic!("Failed to read {}: {e}", header_path.display());
        });
        let variants = parse_enum(&content, "request", "REQ_");
        variants
            .into_iter()
            .map(|(name, index)| RequestDef {
                name,
                index,
                req_fields: vec![],
                reply_fields: vec![],
                has_reply: false,
                req_varargs: vec![],
                reply_varargs: vec![],
            })
            .collect()
    } else {
        vec![]
    };

    // 1. RequestCode enum + from_i32() + as_str()
    let variants: Vec<(String, i32)> = requests.iter().map(|r| (r.name.clone(), r.index)).collect();
    generate_request_code(&mut out, &variants);

    // 2. MSG_* constants from server_protocol.h
    let struct_sizes = if header_path.exists() {
        let content = fs::read_to_string(&header_path).unwrap_or_else(|e| {
            panic!("Failed to read {}: {e}", header_path.display());
        });
        let msg_variants = parse_enum(&content, "message_type", "MSG_");
        generate_message_type_constants(&mut out, &msg_variants);
        parse_struct_sizes(&content)
    } else {
        vec![]
    };

    // 3. Request/reply structs
    generate_request_structs(&mut out, &requests, &struct_sizes);

    // 4. RequestHandler trait + dispatch function
    generate_handler_trait(&mut out, &requests);
    generate_dispatch_function(&mut out, &requests);

    out
}

// ── Full protocol.def parser ─────────────────────────────────────────────────
// Parses @REQ/@REPLY/@END blocks to extract field definitions.

fn parse_protocol_def_full(content: &str, types: &HashMap<&str, (&str, usize, usize)>) -> Vec<RequestDef> {
    let mut requests = Vec::new();
    let mut index: i32 = 0;

    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        if let Some(rest) = trimmed.strip_prefix("@REQ(") {
            if let Some(name) = rest.strip_suffix(')') {
                let name = name.to_string();
                i += 1;

                // Parse request fields until @REPLY or @END
                let mut req_fields = Vec::new();
                let mut reply_fields = Vec::new();
                let mut has_reply = false;
                let mut req_varargs = Vec::new();
                let mut reply_varargs = Vec::new();
                let mut in_reply = false;

                while i < lines.len() {
                    let line = lines[i].trim();

                    if line == "@END" {
                        i += 1;
                        break;
                    }

                    if line == "@REPLY" {
                        has_reply = true;
                        in_reply = true;
                        i += 1;
                        continue;
                    }

                    // Skip comments-only lines and empty lines
                    if line.is_empty() || line.starts_with("/*") || line.starts_with("*") || line.starts_with("//") {
                        i += 1;
                        continue;
                    }

                    // Check for VARARG
                    if line.starts_with("VARARG(") {
                        let vararg_str = line.trim_end_matches(';').to_string();
                        if in_reply {
                            reply_varargs.push(vararg_str);
                        } else {
                            req_varargs.push(vararg_str);
                        }
                        i += 1;
                        continue;
                    }

                    // Parse field: "type  name;  /* comment */"
                    if let Some(field) = parse_field(line, types) {
                        if in_reply {
                            reply_fields.push(field);
                        } else {
                            req_fields.push(field);
                        }
                    }

                    i += 1;
                }

                requests.push(RequestDef {
                    name,
                    index,
                    req_fields,
                    reply_fields,
                    has_reply,
                    req_varargs,
                    reply_varargs,
                });
                index += 1;
                continue;
            }
        }

        i += 1;
    }

    requests
}

/// Parse a single field line like "obj_handle_t  handle;  /* comment */"
fn parse_field(line: &str, types: &HashMap<&str, (&str, usize, usize)>) -> Option<Field> {
    // Strip trailing comment
    let line = if let Some(pos) = line.find("/*") {
        line[..pos].trim()
    } else {
        line.trim()
    };
    // Strip trailing semicolon
    let line = line.trim_end_matches(';').trim();

    if line.is_empty() {
        return None;
    }

    // Try compound types first ("struct X name" or "union X name")
    if line.starts_with("struct ") || line.starts_with("union ") {
        // e.g. "union apc_call call" or "struct rectangle rect"
        let parts: Vec<&str> = line.splitn(3, char::is_whitespace).collect();
        if parts.len() >= 3 {
            let compound_key = format!("{} {}", parts[0], parts[1]);
            let field_name = parts[2].trim();
            if let Some(&(rust_type, size, align)) = types.get(compound_key.as_str()) {
                return Some(Field {
                    name: sanitize_field_name(field_name),
                    rust_type: rust_type.to_string(),
                    size,
                    align,
                });
            }
            // Unknown compound type -- skip with warning
            println!("cargo:warning=  Unknown compound type: {compound_key}");
            return None;
        }
    }

    // Simple types: split into type + name
    // Handle "unsigned int name", "unsigned short name", "unsigned __int64 name"
    let (type_str, name_str) = if line.starts_with("unsigned ") {
        let rest = &line["unsigned ".len()..];
        // Could be "unsigned int name", "unsigned short name", "unsigned __int64 name", "unsigned char name"
        let parts: Vec<&str> = rest.splitn(2, char::is_whitespace).collect();
        if parts.len() >= 2 {
            let second = parts[0].trim();
            let remainder = parts[1].trim();
            // Check if the second word is part of the type
            let candidate = format!("unsigned {second}");
            (candidate, remainder.to_string())
        } else {
            return None;
        }
    } else if line.starts_with("short ") {
        let rest = &line["short ".len()..];
        let parts: Vec<&str> = rest.splitn(2, char::is_whitespace).collect();
        if parts.len() >= 2 {
            let candidate = format!("short {}", parts[0].trim());
            (candidate, parts[1].trim().to_string())
        } else {
            return None;
        }
    } else {
        // Simple: "type_name  field_name"
        let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
        if parts.len() >= 2 {
            (parts[0].trim().to_string(), parts[1].trim().to_string())
        } else {
            return None;
        }
    };

    if let Some(&(rust_type, size, align)) = types.get(type_str.as_str()) {
        Some(Field {
            name: sanitize_field_name(&name_str),
            rust_type: rust_type.to_string(),
            size,
            align,
        })
    } else {
        println!("cargo:warning=  Unknown type: {type_str} (field: {name_str})");
        None
    }
}

/// Sanitize field names that are Rust keywords
fn sanitize_field_name(name: &str) -> String {
    match name {
        "type" => "r#type".to_string(),
        "async" => "r#async".to_string(),
        "move" => "r#move".to_string(),
        "ref" => "r#ref".to_string(),
        "mod" => "r#mod".to_string(),
        "match" => "r#match".to_string(),
        // self/super/crate cannot use r# escape -- rename instead
        "self" => "is_self".to_string(),
        "super" => "is_super".to_string(),
        "crate" => "is_crate".to_string(),
        "return" => "r#return".to_string(),
        "where" => "r#where".to_string(),
        "while" => "r#while".to_string(),
        "for" => "r#for".to_string(),
        "loop" => "r#loop".to_string(),
        "if" => "r#if".to_string(),
        "else" => "r#else".to_string(),
        "fn" => "r#fn".to_string(),
        "use" => "r#use".to_string(),
        "in" => "r#in".to_string(),
        _ => name.to_string(),
    }
}

// ── Struct generation ────────────────────────────────────────────────────────

fn generate_request_structs(
    out: &mut String,
    requests: &[RequestDef],
    struct_sizes: &[(String, usize)],
) {
    out.push_str("// ── Request/Reply structs ─────────────────────────────────────────────────\n");
    out.push_str("// Auto-generated from protocol.def @REQ/@REPLY blocks.\n");
    out.push_str("// Padding matches Wine's make_requests (8-byte struct alignment).\n\n");

    let size_map: HashMap<&str, usize> = struct_sizes
        .iter()
        .map(|(name, size)| (name.as_str(), *size))
        .collect();

    for req in requests {
        let pascal = snake_to_pascal(&req.name);

        // Request struct
        let req_struct_name = format!("{pascal}Request");
        let c_req_name = format!("{}_request", req.name);

        out.push_str("#[repr(C)]\n#[derive(Clone, Copy, Debug)]\n#[allow(dead_code)]\n");
        out.push_str(&format!("pub struct {req_struct_name} {{\n"));
        out.push_str("    pub header: super::RequestHeader,\n");

        let mut offset: usize = 12; // sizeof(RequestHeader)
        let mut pad_idx = 0;

        for field in &req.req_fields {
            // Insert padding if needed for alignment
            if offset % field.align != 0 {
                let pad = field.align - (offset % field.align);
                out.push_str(&format!("    pub _pad_{pad_idx}: [u8; {pad}],\n"));
                offset += pad;
                pad_idx += 1;
            }
            out.push_str(&format!("    pub {}: {},\n", field.name, field.rust_type));
            offset += field.size;
        }

        // Pad to 8-byte alignment at end
        if offset % 8 != 0 {
            let pad = 8 - (offset % 8);
            out.push_str(&format!("    pub _pad_{pad_idx}: [u8; {pad}],\n"));
            offset += pad;
        }

        // VARARG comments
        for v in &req.req_varargs {
            out.push_str(&format!("    // {v}\n"));
        }

        out.push_str("}\n");

        // Size assertion: use C_ASSERT from server_protocol.h if available,
        // otherwise assert our own computed size (validates padding algorithm)
        let req_expected = size_map
            .get(c_req_name.as_str())
            .copied()
            .unwrap_or(offset);
        out.push_str(&format!(
            "const _: () = assert!(std::mem::size_of::<{req_struct_name}>() == {req_expected});\n"
        ));
        out.push('\n');

        // Reply struct
        let reply_struct_name = format!("{pascal}Reply");
        let c_reply_name = format!("{}_reply", req.name);

        out.push_str("#[repr(C)]\n#[derive(Clone, Copy, Debug)]\n#[allow(dead_code)]\n");
        out.push_str(&format!("pub struct {reply_struct_name} {{\n"));
        out.push_str("    pub header: super::ReplyHeader,\n");

        let mut offset: usize = 8; // sizeof(ReplyHeader)
        let mut pad_idx = 0;

        if req.has_reply {
            for field in &req.reply_fields {
                if offset % field.align != 0 {
                    let pad = field.align - (offset % field.align);
                    out.push_str(&format!("    pub _pad_{pad_idx}: [u8; {pad}],\n"));
                    offset += pad;
                    pad_idx += 1;
                }
                out.push_str(&format!("    pub {}: {},\n", field.name, field.rust_type));
                offset += field.size;
            }

            // Pad to 8-byte alignment at end
            if offset % 8 != 0 {
                let pad = 8 - (offset % 8);
                out.push_str(&format!("    pub _pad_{pad_idx}: [u8; {pad}],\n"));
                offset += pad;
            }

            for v in &req.reply_varargs {
                out.push_str(&format!("    // {v}\n"));
            }
        }

        out.push_str("}\n");

        let reply_expected = size_map
            .get(c_reply_name.as_str())
            .copied()
            .unwrap_or(offset);
        out.push_str(&format!(
            "const _: () = assert!(std::mem::size_of::<{reply_struct_name}>() == {reply_expected});\n"
        ));
        out.push('\n');
    }
}

// ── Handler trait generation ─────────────────────────────────────────────────

fn generate_handler_trait(out: &mut String, requests: &[RequestDef]) {
    out.push_str("// ── RequestHandler trait ──────────────────────────────────────────────────\n");
    out.push_str("// Default implementations return STATUS_NOT_IMPLEMENTED (0xC0000002).\n");
    out.push_str("// Override specific methods in your EventLoop impl to handle them.\n\n");

    out.push_str("pub trait RequestHandler {\n");

    for req in requests {
        let handler_name = format!("handle_{}", req.name);
        out.push_str(&format!(
            "    fn {handler_name}(&mut self, _client_fd: i32, _buf: &[u8]) -> crate::event_loop::Reply {{\n\
             \x20       unimplemented_reply()\n\
             \x20   }}\n"
        ));
    }

    out.push_str("}\n\n");

    // Helper function
    out.push_str(
        "fn unimplemented_reply() -> crate::event_loop::Reply {\n\
         \x20   let hdr = super::ReplyHeader { error: 0xC000_0002, reply_size: 0 };\n\
         \x20   let size = std::mem::size_of::<super::ReplyHeader>();\n\
         \x20   let mut buf = [0u8; 64];\n\
         \x20   unsafe {\n\
         \x20       std::ptr::copy_nonoverlapping(\n\
         \x20           &hdr as *const _ as *const u8,\n\
         \x20           buf.as_mut_ptr(),\n\
         \x20           size,\n\
         \x20       );\n\
         \x20   }\n\
         \x20   crate::event_loop::Reply::Fixed { buf, len: size }\n\
         }\n\n",
    );
}

// ── Dispatch function generation ─────────────────────────────────────────────

fn generate_dispatch_function(out: &mut String, requests: &[RequestDef]) {
    out.push_str("// ── Dispatch ─────────────────────────────────────────────────────────────\n\n");

    out.push_str(
        "pub fn dispatch_request(\n\
         \x20   code: super::RequestCode,\n\
         \x20   handler: &mut impl RequestHandler,\n\
         \x20   client_fd: i32,\n\
         \x20   buf: &[u8],\n\
         ) -> crate::event_loop::Reply {\n\
         \x20   match code {\n",
    );

    for req in requests {
        let pascal = snake_to_pascal(&req.name);
        let handler_name = format!("handle_{}", req.name);
        out.push_str(&format!(
            "        super::RequestCode::{pascal} => handler.{handler_name}(client_fd, buf),\n"
        ));
    }

    out.push_str("    }\n}\n\n");
}

// ── Existing generators (kept) ───────────────────────────────────────────────

/// Parse a C enum from server_protocol.h.
fn parse_enum(content: &str, enum_name: &str, prefix: &str) -> Vec<(String, i32)> {
    let mut variants = Vec::new();

    let pattern = format!("enum {enum_name}");
    let start = match content.find(&pattern) {
        Some(pos) => pos,
        None => return variants,
    };

    let brace_start = match content[start..].find('{') {
        Some(pos) => start + pos + 1,
        None => return variants,
    };

    let brace_end = match content[brace_start..].find('}') {
        Some(pos) => brace_start + pos,
        None => return variants,
    };

    let body = &content[brace_start..brace_end];

    let mut index: i32 = 0;
    for token in body.split(',') {
        let token = token.trim();
        let token = if let Some(pos) = token.find("/*") {
            token[..pos].trim()
        } else {
            token
        };
        let token = if let Some(pos) = token.find("//") {
            token[..pos].trim()
        } else {
            token
        };

        if token.is_empty() {
            continue;
        }

        if token.ends_with("nb_requests") {
            continue;
        }

        let name = if token.starts_with(prefix) {
            token[prefix.len()..].to_string()
        } else {
            token.to_string()
        };

        variants.push((name, index));
        index += 1;
    }

    variants
}

/// Parse C_ASSERT(sizeof(struct X) == N) lines from server_protocol.h.
fn parse_struct_sizes(content: &str) -> Vec<(String, usize)> {
    let mut sizes = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("C_ASSERT") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("C_ASSERT(") {
            if let Some(rest) = rest.strip_prefix(" sizeof(") {
                let rest = rest.trim();
                let rest = rest.strip_prefix("struct ").unwrap_or(rest);
                let rest = rest.trim();
                if let Some(paren_pos) = rest.find(')') {
                    let struct_name = rest[..paren_pos].trim().to_string();
                    let after = &rest[paren_pos + 1..];
                    if let Some(eq_pos) = after.find("==") {
                        let num_str = after[eq_pos + 2..].trim();
                        let num_str = num_str.trim_end_matches(|c: char| {
                            c == ')' || c == ';' || c.is_whitespace()
                        });
                        if let Ok(size) = num_str.parse::<usize>() {
                            sizes.push((struct_name, size));
                        }
                    }
                }
            }
        }
    }

    sizes
}

/// Convert snake_case to PascalCase.
fn snake_to_pascal(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    let upper: String = first.to_uppercase().collect();
                    upper + &chars.collect::<String>()
                }
            }
        })
        .collect()
}

fn generate_request_code(out: &mut String, variants: &[(String, i32)]) {
    out.push_str("/// Request opcodes from Wine's server protocol.\n");
    out.push_str("/// Auto-generated from protocol.def @REQ entries.\n");
    out.push_str("#[repr(i32)]\n");
    out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n");
    out.push_str("#[allow(dead_code)]\n");
    out.push_str("pub enum RequestCode {\n");

    for (name, value) in variants {
        let pascal = snake_to_pascal(name);
        out.push_str(&format!("    {pascal} = {value},\n"));
    }

    out.push_str("}\n\n");

    out.push_str("impl RequestCode {\n");
    out.push_str("    #[allow(dead_code)]\n");
    out.push_str("    pub fn from_i32(val: i32) -> Option<Self> {\n");
    out.push_str("        match val {\n");

    for (name, value) in variants {
        let pascal = snake_to_pascal(name);
        out.push_str(&format!("            {value} => Some(Self::{pascal}),\n"));
    }

    out.push_str("            _ => None,\n");
    out.push_str("        }\n");
    out.push_str("    }\n\n");

    out.push_str("    #[allow(dead_code)]\n");
    out.push_str("    pub fn as_str(self) -> &'static str {\n");
    out.push_str("        match self {\n");

    for (name, _) in variants {
        let pascal = snake_to_pascal(name);
        out.push_str(&format!("            Self::{pascal} => \"{name}\",\n"));
    }

    out.push_str("        }\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");
}

fn generate_message_type_constants(out: &mut String, variants: &[(String, i32)]) {
    out.push_str("// Message type constants from enum message_type in server_protocol.h.\n");

    for (name, value) in variants {
        let const_name = format!("MSG_{}", name.to_uppercase());
        out.push_str(&format!(
            "#[allow(dead_code)]\npub const {const_name}: i32 = {value};\n"
        ));
    }

    out.push('\n');
}
