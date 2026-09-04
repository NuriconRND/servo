/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use malloc_size_of_derive::MallocSizeOf;
use uuid::Uuid;

use super::MediaStream;

type RegisteredMediaStream = Arc<Mutex<dyn MediaStream>>;

static MEDIA_STREAMS_REGISTRY: LazyLock<Mutex<HashMap<MediaStreamId, RegisteredMediaStream>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Copy, Hash, Eq, PartialEq, MallocSizeOf)]
pub struct MediaStreamId(Uuid);
impl MediaStreamId {
    pub fn new() -> MediaStreamId {
        Self(Uuid::new_v4())
    }

    pub fn id(self) -> Uuid {
        self.0
    }
}

impl Default for MediaStreamId {
    fn default() -> Self {
        Self::new()
    }
}

pub fn register_stream(stream: Arc<Mutex<dyn MediaStream>>) -> MediaStreamId {
    let id = MediaStreamId::new();
    stream.lock().unwrap().set_id(id);
    MEDIA_STREAMS_REGISTRY.lock().unwrap().insert(id, stream);
    id
}

pub fn unregister_stream(stream: &MediaStreamId) {
    // 잠금을 놓은 뒤에 스트림을 drop 한다. `MediaStream` 구현체의 Drop 은
    // 관례적으로 자기 자신을 다시 unregister 하므로(GStreamerMediaStream),
    // 잠금을 쥔 채 drop 하면 같은 뮤텍스를 재진입해 교착한다. 반환값을
    // 이름 있는 바인딩으로 받아야 MutexGuard 가 먼저 풀린다 — 결과를 버리면
    // 임시값 drop 순서상 Arc 가 guard 보다 먼저 죽어 교착한다.
    let removed = MEDIA_STREAMS_REGISTRY.lock().unwrap().remove(stream);
    drop(removed);
}

pub fn get_stream(stream: &MediaStreamId) -> Option<Arc<Mutex<dyn MediaStream>>> {
    MEDIA_STREAMS_REGISTRY.lock().unwrap().get(stream).cloned()
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::{MediaStream, MediaStreamType};

    /// `GStreamerMediaStream` 과 같은 모양: Drop 에서 자기 자신을 다시 unregister 한다.
    struct ReentrantStream {
        id: Option<MediaStreamId>,
    }

    impl MediaStream for ReentrantStream {
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn as_mut_any(&mut self) -> &mut dyn Any {
            self
        }
        fn set_id(&mut self, id: MediaStreamId) {
            self.id = Some(id);
        }
        fn ty(&self) -> MediaStreamType {
            MediaStreamType::Video
        }
    }

    impl Drop for ReentrantStream {
        fn drop(&mut self) {
            if let Some(ref id) = self.id {
                unregister_stream(id);
            }
        }
    }

    #[test]
    fn unregistering_a_stream_that_unregisters_itself_does_not_deadlock() {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let id = register_stream(Arc::new(Mutex::new(ReentrantStream { id: None })));
            unregister_stream(&id);
            let _ = tx.send(id);
        });
        let id = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("unregister_stream deadlocked on the re-entrant Drop");
        assert!(get_stream(&id).is_none());
    }
}
