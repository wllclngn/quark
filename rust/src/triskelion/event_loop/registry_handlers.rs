// Registry opcode handlers — thin wrappers around registry.rs

use super::*;

impl EventLoop {

    // ---- Registry handlers ----

    pub(crate) fn handle_create_key(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        if buf.len() < std::mem::size_of::<CreateKeyRequest>() {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        }

        let vararg = &buf[VARARG_OFF..];
        let (rootdir, name) = crate::registry::parse_objattr_name(vararg);
        let parent = if rootdir != 0 { rootdir } else { 0 };
        // DIAG: trace every create_key to find display device registry bug
        let _name_str = crate::registry::utf16le_to_string_pub(name);
        let _parent_path = if parent != 0 {
            self.registry.get_handle_path(parent).unwrap_or_else(|| "<unknown>".to_string())
        } else {
            "<root>".to_string()
        };
        let (hkey, created) = self.registry.create_key(parent, name);

        if created && parent != 0 {
            self.fire_registry_notifications(parent, 0x01); // REG_NOTIFY_CHANGE_NAME
        }

        // Stock wineserver returns STATUS_OBJECT_NAME_EXISTS (0x40000000) when
        // the key already existed. Wine's ntdll uses this to distinguish
        // REG_CREATED_NEW_KEY vs REG_OPENED_EXISTING_KEY in NtCreateKey.
        let error = if created { 0 } else { 0x40000000 };
        let reply = CreateKeyReply {
            header: ReplyHeader { error, reply_size: 0 },
            hkey,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_open_key(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<OpenKeyRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const OpenKeyRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let vararg = &buf[VARARG_OFF..];
        // open_key VARARG is just unicode_str (no objattr wrapper)
        let name_str = crate::registry::utf16le_to_string_pub(vararg);
        if let Some(hkey) = self.registry.open_key(req.parent, vararg) {
            let path = self.registry.get_handle_path(hkey).unwrap_or_else(|| "<unknown>".to_string());
            log_info!("open_key: '{name_str}' -> {path} (hkey={hkey})");
            let reply = OpenKeyReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                hkey,
                _pad_0: [0; 4],
            };
            reply_fixed(&reply)
        } else {
            log_info!("open_key: '{name_str}' NOT FOUND (parent={:#x})", req.parent);
            reply_fixed(&ReplyHeader { error: 0xC0000034, reply_size: 0 }) // STATUS_OBJECT_NAME_NOT_FOUND
        }
    }


    pub(crate) fn handle_get_key_value(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetKeyValueRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetKeyValueRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let vararg = &buf[VARARG_OFF..];
        if let Some((data_type, data)) = self.registry.get_value(req.hkey, vararg) {
            let max = max_reply_vararg(buf) as usize;
            let send_len = data.len().min(max);
            let reply = GetKeyValueReply {
                header: ReplyHeader { error: 0, reply_size: send_len as u32 },
                r#type: data_type as i32,
                total: data.len() as u32,
            };
            reply_vararg(&reply, &data[..send_len])
        } else {
            // Stock wineserver returns the full GetKeyValueReply with type=-1
            // (0xFFFFFFFF) when the value is not found. Wine's ntdll reads
            // the type field even on error.
            reply_fixed(&GetKeyValueReply {
                header: ReplyHeader { error: 0xC0000034, reply_size: 0 },
                r#type: -1,
                total: 0,
            })
        }
    }


    pub(crate) fn handle_set_key_value(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetKeyValueRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetKeyValueRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let vararg = &buf[VARARG_OFF..];
        let namelen = req.namelen as usize;
        if vararg.len() >= namelen {
            let name = &vararg[..namelen];
            let data = &vararg[namelen..];
            let name_str = crate::registry::utf16le_to_string_pub(name);
            let path = self.registry.get_handle_path(req.hkey).unwrap_or_else(|| "<unknown>".to_string());
            log_info!("set_key_value: '{name_str}' in {path} type={}", req.r#type);
            self.registry.set_value(req.hkey, name, req.r#type as u32, data);
            self.fire_registry_notifications(req.hkey, 0x04); // REG_NOTIFY_CHANGE_LAST_SET

            // Note: __wine_display_device_guid is pre-set at daemon startup with a
            // deterministic null GUID. No need to intercept explorer's dynamic GUID here.
        }

        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_enum_key_value(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<EnumKeyValueRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const EnumKeyValueRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        if let Some((name_bytes, data_type, data)) = self.registry.enum_value(req.hkey, req.index as usize) {
            let max = max_reply_vararg(buf) as usize;
            let mut vararg = name_bytes.clone();
            vararg.extend_from_slice(data);
            let send_len = vararg.len().min(max);
            if send_len < vararg.len() {
            }

            let reply = EnumKeyValueReply {
                header: ReplyHeader { error: 0, reply_size: send_len as u32 },
                r#type: data_type as i32,
                total: data.len() as u32,
                namelen: name_bytes.len() as u32,
                _pad_0: [0; 4],
            };
            reply_vararg(&reply, &vararg[..send_len])
        } else {
            reply_fixed(&ReplyHeader { error: 0x8000001A, reply_size: 0 }) // STATUS_NO_MORE_ENTRIES
        }
    }


    // ---- Startup stubs (no-op success) ----

    pub(crate) fn handle_load_registry(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_flush_key(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&FlushKeyReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
        })
    }


    pub(crate) fn handle_enum_key(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<EnumKeyRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const EnumKeyRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let _path_str = self.registry.get_handle_path(req.hkey).unwrap_or_else(|| "<unknown>".to_string());

        // index == -1 means NtQueryKey (query key metadata), not NtEnumerateKey
        if req.index == -1 {
            if let Some((subkeys, values, max_value, max_data)) = self.registry.query_key(req.hkey) {
                let reply = EnumKeyReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    subkeys,
                    max_subkey: 0,
                    max_class: 0,
                    values,
                    max_value: max_value as i32,
                    max_data: max_data as i32,
                    // LastWriteTime: Wine's display cache compares this against
                    // last_query_display_time. Use the registry's global write counter
                    // so the timestamp only changes when a value is actually written.
                    modif: self.registry.write_counter() as i64,
                    total: 0,
                    namelen: 0,
                };
                return reply_fixed(&reply);
            } else {
                return reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 }); // STATUS_INVALID_HANDLE
            }
        }

        let _child_count = self.registry.query_key(req.hkey).map(|(s, _, _, _)| s).unwrap_or(-1);
        if let Some((name_bytes, subkeys, values)) = self.registry.enum_key(req.hkey, req.index as usize) {
            let max = max_reply_vararg(buf) as usize;
            let send_len = name_bytes.len().min(max);

            let reply = EnumKeyReply {
                header: ReplyHeader { error: 0, reply_size: send_len as u32 },
                subkeys,
                max_subkey: 0,
                max_class: 0,
                values,
                max_value: 0,
                max_data: 0,
                modif: 0,
                total: name_bytes.len() as u32,
                namelen: name_bytes.len() as u32,
            };
            reply_vararg(&reply, &name_bytes[..send_len])
        } else {
            reply_fixed(&ReplyHeader { error: 0x8000001A, reply_size: 0 }) // STATUS_NO_MORE_ENTRIES
        }
    }


    pub(crate) fn handle_delete_key(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let hkey = if buf.len() >= 16 {
            u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]])
        } else { 0 };

        if hkey != 0 && self.registry.delete_key(hkey) {
            self.fire_registry_notifications(hkey, 0x01); // REG_NOTIFY_CHANGE_NAME
            reply_fixed(&DeleteKeyReply { header: ReplyHeader { error: 0, reply_size: 0 } })
        } else {
            // STATUS_ACCESS_DENIED if has subkeys, or key not found
            reply_fixed(&DeleteKeyReply { header: ReplyHeader { error: 0xC0000022, reply_size: 0 } })
        }
    }


    pub(crate) fn handle_delete_key_value(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let hkey = if buf.len() >= 16 {
            u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]])
        } else { 0 };
        // Value name is in VARARG (UTF-16LE after the fixed struct)
        let name = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };

        if hkey != 0 && self.registry.delete_value(hkey, name) {
            self.fire_registry_notifications(hkey, 0x04); // REG_NOTIFY_CHANGE_LAST_SET
            reply_fixed(&DeleteKeyValueReply { header: ReplyHeader { error: 0, reply_size: 0 } })
        } else {
            reply_fixed(&DeleteKeyValueReply { header: ReplyHeader { error: 0xC0000034, reply_size: 0 } }) // NOT_FOUND
        }
    }


    pub(crate) fn handle_unload_registry(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&UnloadRegistryReply { header: ReplyHeader { error: 0, reply_size: 0 } })
    }


    pub(crate) fn handle_save_registry(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&SaveRegistryReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
        })
    }


    pub(crate) fn handle_set_registry_notification(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetRegistryNotificationRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetRegistryNotificationRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let pid = self.client_pid(client_fd as RawFd);

        // Reset the event so it's unsignaled while waiting
        if let Some((obj, _)) = self.ntsync_objects.get(&(pid, req.event)) {
            let _ = obj.event_reset();
        }

        if self.registry.register_notify(req.hkey, pid, req.event, req.subtree != 0, req.filter) {
            // STATUS_PENDING: notification registered, event will be signaled on change
            reply_fixed(&SetRegistryNotificationReply {
                header: ReplyHeader { error: 0x103, reply_size: 0 },
            })
        } else {
            reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 }) // STATUS_INVALID_HANDLE
        }
    }

    // Signal ntsync events for registry notifications that match a mutation.
    fn fire_registry_notifications(&mut self, changed_hkey: u32, change: u32) {
        let fired = self.registry.collect_notifications(changed_hkey, change);
        for (pid, event_handle) in fired {
            if let Some((obj, _)) = self.ntsync_objects.get(&(pid, event_handle)) {
                let _ = obj.event_set();
            }
        }
    }


    // ---- Atom table ----

    pub(crate) fn handle_add_atom(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let vararg = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
        let name_u16 = vararg_to_u16(vararg);

        // Check if atom already exists
        if let Some(&atom) = self.state.atom_names.get(&name_u16) {
            if let Some(entry) = self.state.atoms.get_mut(&atom) {
                entry.1 += 1; // bump refcount
            }
            let reply = AddAtomReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                atom,
                _pad_0: [0; 4],
            };
            return reply_fixed(&reply);
        }

        let atom = self.state.next_atom;
        self.state.next_atom += 1;
        self.state.atoms.insert(atom, (vararg.to_vec(), 1));
        self.state.atom_names.insert(name_u16, atom);

        let reply = AddAtomReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            atom,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_add_user_atom(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        // Same as add_atom but for user-handle atoms (display driver window classes).
        // Wine's win32u uses these for class registration — must return non-zero atoms.
        let vararg = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
        let name_u16 = vararg_to_u16(vararg);

        if let Some(&atom) = self.state.atom_names.get(&name_u16) {
            if let Some(entry) = self.state.atoms.get_mut(&atom) {
                entry.1 += 1;
            }
            return reply_fixed(&AddAtomReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                atom,
                _pad_0: [0; 4],
            });
        }

        let atom = self.state.next_atom;
        self.state.next_atom += 1;
        self.state.atoms.insert(atom, (vararg.to_vec(), 1));
        self.state.atom_names.insert(name_u16, atom);

        reply_fixed(&AddAtomReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            atom,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_find_atom(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let vararg = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
        let name_u16 = vararg_to_u16(vararg);

        if let Some(&atom) = self.state.atom_names.get(&name_u16) {
            let reply = FindAtomReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                atom,
                _pad_0: [0; 4],
            };
            return reply_fixed(&reply);
        }

        reply_fixed(&ReplyHeader { error: 0xC0000034, reply_size: 0 }) // NOT_FOUND
    }


    pub(crate) fn handle_get_atom_information(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetAtomInformationRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetAtomInformationRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let max_vararg = max_reply_vararg(buf);

        if let Some((name_bytes, count)) = self.state.atoms.get(&req.atom) {
            let vararg_len = name_bytes.len().min(max_vararg as usize);
            let reply = GetAtomInformationReply {
                header: ReplyHeader { error: 0, reply_size: vararg_len as u32 },
                count: *count,
                pinned: 0,
                total: name_bytes.len() as u32,
                _pad_0: [0; 4],
            };
            return reply_vararg(&reply, &name_bytes[..vararg_len]);
        }

        reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 }) // INVALID_HANDLE
    }


    pub(crate) fn handle_delete_atom(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<DeleteAtomRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const DeleteAtomRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        if let Some(entry) = self.state.atoms.get_mut(&req.atom) {
            entry.1 -= 1;
            if entry.1 <= 0 {
                let name_bytes = self.state.atoms.remove(&req.atom).unwrap().0;
                let name_u16 = vararg_to_u16(&name_bytes);
                self.state.atom_names.remove(&name_u16);
            }
        }

        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }
}
