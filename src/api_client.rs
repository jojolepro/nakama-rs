//! Stateful nakama client, abstracting over nakama api and rt_api.

use crate::{
    api::{self, RestRequest},
    async_client::AsyncRequestTick,
    rt_api::{Presence, Socket, SocketEvent},
};

use nanoserde::DeJson;
use std::{cell::RefCell, collections::HashMap, rc::Rc};

pub enum Event {
    Presence {
        joins: Vec<Presence>,
        leaves: Vec<Presence>,
    },
    MatchData {
        data: Vec<u8>,
        opcode: i32,
        user_id: String,
    },
}

pub struct NakamaState {
    server_url: String,
    ws_url: String,
    port: u32,

    pub socket: Option<Socket>,
    pub username: Option<String>,
    pub token: Option<String>,
    pub refresh_token: Option<String>,
    pub match_id: Option<String>,
    pub rpc_response: Option<String>,
    pub error: Option<String>,
    pub next_request: Option<Box<dyn AsyncRequestTick>>,
}

impl NakamaState {
    pub fn reset(&mut self) {
        self.socket = None;
        self.username = None;
        self.token = None;
        self.refresh_token = None;
        self.match_id = None;
        self.error = None;
    }

    pub fn make_request<T, F>(&mut self, request: RestRequest<T>, on_success: F)
    where
        T: nanoserde::DeJson + 'static,
        F: FnMut(T) -> () + 'static,
    {
        assert!(self.next_request.is_none());

        let mut request = crate::async_client::make_request(&self.server_url, self.port, request);
        request.on_success(on_success);
        self.next_request = Some(Box::new(request));
    }
}

/// Statefull, non-blocking nakama client.
/// Works as a state machine - all calls are non-blocking, but may modify some
/// internal ApiClient state and therefore results of other calls in the future.
pub struct ApiClient {
    key: String,
    events: Vec<Event>,
    pub session_id: Option<String>,
    pub matchmaker_token: Option<String>,
    state: Rc<RefCell<NakamaState>>,
    ongoing_request: Option<Box<dyn AsyncRequestTick>>,
    socket_response: HashMap<u32, SocketEvent>,
}

impl ApiClient {
    pub fn new(key: &str, server: &str, port: u32, protocol: &str) -> ApiClient {
        ApiClient {
            key: key.to_owned(),
            state: Rc::new(RefCell::new(NakamaState {
                ws_url: match protocol {
                    "http" => format!("ws://{}", server.to_owned()),
                    "https" => format!("wss://{}", server.to_owned()),
                    _ => panic!("Unsupported protocol"),
                },

                server_url: match protocol {
                    "http" => format!("http://{}", server.to_owned()),
                    "https" => format!("https://{}", server.to_owned()),
                    _ => panic!("Unsupported protocol"),
                },
                port,
                socket: None,
                token: None,
                refresh_token: None,
                rpc_response: None,
                error: None,
                username: None,
                match_id: None,
                next_request: None,
            })),
            socket_response: HashMap::new(),
            ongoing_request: None,
            events: vec![],
            session_id: None,
            matchmaker_token: None,
        }
    }

    pub fn in_progress(&self) -> bool {
        self.ongoing_request.is_some()
    }

    pub fn authenticate(&mut self, email: &str, password: &str) {
        self.session_id = None;
        self.state.borrow_mut().socket = None;
        self.state.borrow_mut().username = None;

        let request = api::authenticate_email(
            &self.key,
            "",
            api::ApiAccountEmail {
                email: email.to_owned(),
                password: password.to_owned(),
                vars: std::collections::HashMap::new(),
            },
            Some(false),
            None,
        );

        self.state.borrow_mut().make_request(request, {
            let state2 = self.state.clone();
            move |session| {
                let mut state = state2.borrow_mut();
                state.socket = Some(Socket::connect(
                    &state.ws_url,
                    state.port,
                    false,
                    &session.token,
                ));
                state.token = Some(session.token);
                state.refresh_token = Some(session.refresh_token);

                let request = api::get_account(&state.token.as_ref().unwrap());
                state.make_request(request, {
                    let state = state2.clone();
                    move |account| {
                        let mut state = state.borrow_mut();
                        state.username = Some(account.user.username);
                    }
                });
            }
        });
    }

    pub fn register(&mut self, email: &str, password: &str, username: &str) {
        let request = api::authenticate_email(
            &self.key,
            "",
            api::ApiAccountEmail {
                email: email.to_owned(),
                password: password.to_owned(),
                vars: std::collections::HashMap::new(),
            },
            Some(true),
            Some(username),
        );

        self.state.borrow_mut().make_request(request, {
            let state2 = self.state.clone();
            move |session| {
                let mut state = state2.borrow_mut();
                state.socket = Some(Socket::connect(
                    &state.ws_url,
                    state.port,
                    false,
                    &session.token,
                ));
                state.token = Some(session.token);

                let request = api::get_account(&state.token.as_ref().unwrap());
                state.make_request(request, {
                    let state = state2.clone();
                    move |account| {
                        let mut state = state.borrow_mut();
                        state.username = Some(account.user.username);
                    }
                });
            }
        });
    }

    pub fn username(&self) -> Option<String> {
        self.state.borrow().username.clone()
    }

    pub fn rpc(&mut self, name: &str, body: &str) {
        self.state.borrow_mut().rpc_response = None;

        let request = api::rpc_func(
            &self.state.borrow().token.as_ref().unwrap(),
            name,
            body,
            None,
        );
        self.state.borrow_mut().make_request(request, {
            let state2 = self.state.clone();
            move |response| {
                state2.borrow_mut().rpc_response = Some(response.payload);
            }
        });
    }

    pub fn logout(&mut self) {
        // let request = api::session_logout(
        //     &self.state.borrow().token.as_ref().unwrap(),
        //     api::ApiSessionLogoutRequest {
        //         token: self.state.borrow().token.clone().unwrap(),
        //         refresh_token: self.state.borrow().refresh_token.clone().unwrap(),
        //     },
        // );
        // self.state.borrow_mut().make_request(request, |_| {});

        // workaround: for some reasone nakama cant process logout request
        // so we reset all nakama data to ensure that next time we will have a new connection
        // but not really notifying the cloud that we want to switch an account
        self.session_id = None;
        self.matchmaker_token = None;
        self.state.borrow_mut().reset();
    }

    pub fn authenticated(&self) -> bool {
        self.state.borrow().username.is_some()
            && self.state.borrow().socket.is_some()
            && self.state.borrow().socket.as_ref().unwrap().connected()
    }

    pub fn try_recv(&mut self) -> Option<Event> {
        self.events.pop()
    }

    pub fn tick(&mut self) {
        let mut state = self.state.borrow_mut();
        if let Some(ref mut socket) = state.socket {
            if let Some(msg) = socket.try_recv() {
                let event: SocketEvent = DeJson::deserialize_json(&msg).unwrap();

                if let Some(ref cid) = event.cid {
                    self.socket_response
                        .insert(cid.parse::<u32>().unwrap(), event.clone());
                }
                if let Some(presence) = event.match_presence_event {
                    self.events.push(Event::Presence {
                        joins: presence.joins.iter().cloned().collect::<Vec<_>>(),
                        leaves: presence.leaves.iter().cloned().collect::<Vec<_>>(),
                    });
                }

                if let Some(new_match) = event.new_match {
                    self.session_id = Some(new_match.self_user.session_id.clone());
                    state.match_id = Some(new_match.match_id.clone());

                    self.events.push(Event::Presence {
                        joins: new_match.presences.clone(),
                        leaves: vec![],
                    });
                }

                if let Some(data) = event.match_data {
                    self.events.push(Event::MatchData {
                        user_id: data.presence.session_id,
                        opcode: data.op_code.parse().unwrap(),
                        data: data.data,
                    });
                }

                if let Some(matched) = event.matchmaker_matched {
                    self.matchmaker_token = Some(matched.token);
                }
            }
        }
        drop(state);

        if let Some(ref mut request) = self.ongoing_request {
            if request.tick() {
                self.ongoing_request = None;
            }
        }

        if let Some(request) = self.state.borrow_mut().next_request.take() {
            assert!(self.ongoing_request.is_none());

            self.ongoing_request = Some(request);
        }
    }

    pub fn match_id(&self) -> Option<String> {
        self.state.borrow().match_id.clone()
    }

    pub fn rpc_response(&self) -> Option<String> {
        self.state.borrow().rpc_response.clone()
    }

    pub fn socket_add_matchmaker(
        &mut self,
        min_count: u32,
        max_count: u32,
        query: &str,
        string_properties: &str,
    ) {
        let mut state = &mut *self.state.borrow_mut();

        self.matchmaker_token = None;
        state.match_id = None;

        state.socket.as_mut().unwrap().add_matchmaker(
            min_count,
            max_count,
            query,
            string_properties,
        );
    }

    pub fn socket_create_match(&mut self) -> u32 {
        self.state
            .borrow_mut()
            .socket
            .as_mut()
            .unwrap()
            .create_match()
    }

    pub fn socket_join_match_by_id(&mut self, match_id: &str) -> u32 {
        self.state
            .borrow_mut()
            .socket
            .as_mut()
            .unwrap()
            .join_match_by_id(match_id)
    }

    pub fn socket_join_match_by_token(&mut self, token_id: &str) -> u32 {
        self.state
            .borrow_mut()
            .socket
            .as_mut()
            .unwrap()
            .join_match_by_token(token_id)
    }
    pub fn socket_leave_match(&mut self) -> u32 {
        let state = &mut *self.state.borrow_mut();

        state
            .socket
            .as_mut()
            .unwrap()
            .leave_match(state.match_id.as_ref().unwrap())
    }

    pub fn socket_send<T: nanoserde::SerBin>(&mut self, opcode: i32, data: &T) {
        let binary_data = nanoserde::SerBin::serialize_bin(data);

        let state = &mut *self.state.borrow_mut();

        state.socket.as_mut().unwrap().match_data_send(
            state.match_id.as_ref().unwrap(),
            opcode,
            &binary_data,
        );
    }

    pub fn socket_response(&self, cid: u32) -> Option<SocketEvent> {
        self.socket_response.get(&cid).cloned()
    }

    pub fn error(&self) -> Option<String> {
        self.state.borrow().error.clone()
    }
}
