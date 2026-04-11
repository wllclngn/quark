// Token, security, and access control handlers

use super::*;

impl EventLoop {

    pub(crate) fn handle_get_token_sid(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let _req = if buf.len() >= std::mem::size_of::<GetTokenSidRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetTokenSidRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let sid = &self.user_sid;

        let max = max_reply_vararg(buf) as usize;

        // Wine's ntdll first calls with a zero-size reply buffer to query the
        // required SID length. If the SID doesn't fit, return STATUS_BUFFER_TOO_SMALL
        // with sid_len set — the client will retry with a larger buffer.
        // Without this, Wine reads past a 0-byte buffer → STATUS_ACCESS_VIOLATION.
        if sid.len() > max {
            let reply = GetTokenSidReply {
                header: ReplyHeader { error: 0xC0000023, reply_size: 0 }, // STATUS_BUFFER_TOO_SMALL
                sid_len: sid.len() as u32,
                _pad_0: [0; 4],
            };
            return reply_fixed(&reply);
        }

        let reply = GetTokenSidReply {
            header: ReplyHeader { error: 0, reply_size: sid.len() as u32 },
            sid_len: sid.len() as u32,
            _pad_0: [0; 4],
        };
        reply_vararg(&reply, &sid)
    }


    pub(crate) fn handle_get_token_default_dacl(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        // Return empty DACL (no restrictions)
        let reply = GetTokenDefaultDaclReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            acl_len: 0,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_open_token(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        let handle = self.alloc_waitable_handle_for_client(client_fd);
        if handle == 0 {
            return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 });
        }
        let reply = OpenTokenReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            token: handle,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_get_token_info(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        let mut token_id = [0u8; 8];
        token_id[0] = 0xE8; token_id[1] = 0x03; // 1000 as u64 LE
        let mut modified_id = [0u8; 8];
        modified_id[0] = 1; // 1 as u64 LE
        let reply = GetTokenInfoReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            token_id,
            modified_id,
            session_id: 0,
            primary: 1,
            impersonation_level: 0,
            elevation_type: 3, // TokenElevationTypeFull (elevated)
            is_elevated: 1,
            group_count: 0,
            privilege_count: 0,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_get_token_groups(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        let reply = GetTokenGroupsReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            attr_len: 0,
            sid_len: 0,
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_get_token_privileges(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        let reply = GetTokenPrivilegesReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            len: 0,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    // Security
    pub(crate) fn handle_access_check(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&AccessCheckReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            access_granted: 0x1F0FFF,
            access_status: 0,
            privileges_len: 0,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_get_security_object(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&GetSecurityObjectReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            sd_len: 0,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_set_security_object(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

}
