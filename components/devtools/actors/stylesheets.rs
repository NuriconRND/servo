/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
use std::sync::Arc;

use devtools_traits::DevtoolScriptControlMsg;
use log::warn;
use malloc_size_of_derive::MallocSizeOf;
use serde::Serialize;
use serde_json::{Map, Value};
use servo_base::generic_channel;
use servo_base::generic_channel::GenericSender;

use crate::StreamId;
use crate::actor::{Actor, ActorError, ActorRegistry, new_actor_name};
use crate::actors::browsing_context::BrowsingContextActor;
use crate::actors::long_string::{LongStringActor, LongStringObj};
use crate::protocol::ClientRequest;

/// <https://searchfox.org/mozilla-central/source/devtools/server/actors/resources/stylesheets.js>
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StyleSheetData {
    /// Unique identifier for this stylesheet.
    pub(crate) resource_id: String,
    /// Current browsing context id.
    pub(crate) browsing_context_id: u32,
    /// The URL of the stylesheet. Optional for inline stylesheets.
    pub(crate) href: Option<String>,
    /// The URL of the document that owns this stylesheet.
    pub(crate) node_href: String,
    /// Whether the stylesheet is disabled.
    pub(crate) disabled: bool,
    /// The title of the stylesheet.
    pub(crate) title: Option<String>,
    /// Whether this is a browser stylesheet.
    pub(crate) system: bool,
    /// Whether this stylesheet was created by DevTools.
    pub(crate) is_new: bool,
    /// Optional file name used for local files.
    pub(crate) file_name: Option<String>,
    /// Optional source map URL.
    #[serde(rename = "sourceMapURL")]
    pub(crate) source_map_url: Option<String>,
    #[serde(rename = "sourceMapBaseURL")]
    pub(crate) source_map_base_url: Option<String>,
    /// The index of this stylesheet in the document's stylesheet list.
    pub(crate) style_sheet_index: i32,
    /// whether the stylesheet was constructed using Web APIs.
    pub(crate) constructed: bool,
    /// Total count of individual CSS rules within that specific stylesheet.
    pub(crate) rule_count: u32,
    /// List of media query metadata (ex: @media, @keyframes).
    pub(crate) at_rules: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GetStyleSheetsReply {
    from: String,
    style_sheets: Vec<StyleSheetData>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GetTextReply {
    from: String,
    text: LongStringObj,
}

#[derive(MallocSizeOf)]
pub(crate) struct StyleSheetsActor {
    name: String,
    script_sender: GenericSender<DevtoolScriptControlMsg>,
    browsing_context_name: String,
}

impl Actor for StyleSheetsActor {
    fn name(&self) -> &str {
        &self.name
    }
    fn handle_message(
        &self,
        request: ClientRequest,
        registry: &ActorRegistry,
        msg_type: &str,
        msg: &Map<String, Value>,
        _id: StreamId,
    ) -> Result<(), ActorError> {
        let browsing_context_actor =
            registry.find::<BrowsingContextActor>(&self.browsing_context_name);
        match msg_type {
            "getStyleSheets" => {
                let style_sheets = self.get_stylesheets_data(&browsing_context_actor);
                let msg = GetStyleSheetsReply {
                    from: self.name().into(),
                    style_sheets,
                };
                request.reply_final(&msg)?
            },
            // TODO: Improve CSS text formatting for remote stylesheets to match as source.
            "getText" => {
                let resource_id = msg.get("resourceId").and_then(|v| v.as_str()).unwrap_or("");
                let index = resource_id
                    .split('-')
                    .next_back()
                    .unwrap_or("0")
                    .parse::<i32>()
                    .unwrap_or(0);
                let (tx, rx) = generic_channel::channel().unwrap();
                let _ = self
                    .script_sender
                    .send(DevtoolScriptControlMsg::GetStyleSheetText(
                        browsing_context_actor.pipeline_id(),
                        index,
                        tx,
                    ));
                let css_text = rx
                    .recv()
                    .map_err(|_| ActorError::Internal)?
                    .unwrap_or_else(|| {
                        warn!("Stylesheet fetched without text content");
                        "Error fetching CSS text".to_string()
                    });
                let long_string_actor = LongStringActor::register(registry, css_text);
                let msg = GetTextReply {
                    from: self.name().into(),
                    text: long_string_actor.long_string_obj(),
                };
                request.reply_final(&msg)?
            },
            _ => return Err(ActorError::UnrecognizedPacketType),
        };
        Ok(())
    }
}

impl StyleSheetsActor {
    pub fn register(
        registry: &ActorRegistry,
        script_sender: GenericSender<DevtoolScriptControlMsg>,
        browsing_context_name: String,
    ) -> Arc<Self> {
        let name = new_actor_name::<Self>();
        let actor = StyleSheetsActor {
            name,
            script_sender,
            browsing_context_name,
        };
        registry.register::<Self>(actor)
    }

    pub(crate) fn get_stylesheets_data(
        &self,
        browsing_context_actor: &BrowsingContextActor,
    ) -> Vec<StyleSheetData> {
        let (tx, rx) = generic_channel::channel().unwrap();
        // 이 액터는 자기가 가리키는 파이프라인보다 오래 살 수 있다(페이지 새로고침이 대표적
        // 이다 - 새 파이프라인이 생겨도 액터는 옛 id 를 들고 있다). 그러면 send 가 실패하고
        // tx 가 그대로 떨어져 recv 는 **반드시** Disconnected 다.
        //
        // 예전에는 그 자리에서 unwrap 해 DevtoolsClientHandler 스레드가 죽었고, 브라우저는
        // 살아남지만 devtools 연결이 끊겨 재접속해야 했다. 스타일 편집기가 비어 보이는 편이
        // 낫다 - 스크립트 쪽 핸들러도 문서를 못 찾으면 빈 목록을 회신한다(devtools.rs 의
        // handle_get_stylesheets). 즉 빈 결과는 이미 정상 경로에 있는 값이다.
        if self
            .script_sender
            .send(DevtoolScriptControlMsg::GetStyleSheets(
                browsing_context_actor.pipeline_id(),
                tx,
            ))
            .is_err()
        {
            warn!("stylesheets: script thread is gone; returning an empty list");
            return vec![];
        }
        let Ok(style_sheets) = rx.recv() else {
            warn!("stylesheets: no reply from the script thread; returning an empty list");
            return vec![];
        };
        let url = browsing_context_actor.url();
        let browsing_context_id = browsing_context_actor.browsing_context_id.value();
        style_sheets
            .into_iter()
            .map(|info| StyleSheetData {
                resource_id: format!("{}-{}", browsing_context_id, info.style_sheet_index),
                browsing_context_id,
                href: info.href.clone(),
                node_href: url.clone(),
                disabled: info.disabled,
                title: (!info.title.is_empty()).then_some(info.title),
                system: info.system,
                is_new: false,
                file_name: None,
                source_map_url: Some("".to_string()),
                source_map_base_url: Some(info.href.unwrap_or_else(|| url.clone())),
                style_sheet_index: info.style_sheet_index,
                constructed: false,
                rule_count: info.rule_count,
                at_rules: vec![], // TODO: Populate with media query metadata for the Style Editor sidebar.
            })
            .collect()
    }
}
