mod store;

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use log::debug;
use tokio::sync::RwLock;

use crate::index_resolver::IndexUid;
use crate::tasks::task::TaskEvent;

use super::error::TaskError;
use super::task::{Task, TaskContent, TaskId};
use super::Result;

#[cfg(test)]
pub use store::test::MockStore as Store;
#[cfg(not(test))]
pub use store::Store;

/// Defines constraints to be applied when querying for Tasks from the store.
#[derive(Default, Debug)]
pub struct TaskFilter {
    indexes: Option<HashSet<String>>,
}

impl TaskFilter {
    fn pass(&self, task: &Task) -> bool {
        self.indexes
            .as_ref()
            .map(|indexes| indexes.contains(&*task.index_uid))
            .unwrap_or(true)
    }

    /// Adds an index to the filter, so the filter must match this index.
    pub fn filter_index(&mut self, index: String) {
        self.indexes
            .get_or_insert_with(Default::default)
            .insert(index);
    }
}

pub struct TaskStore {
    store: Arc<Store>,
    pending_queue: Arc<RwLock<BinaryHeap<Reverse<TaskId>>>>,
}

impl Clone for TaskStore {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            pending_queue: self.pending_queue.clone(),
        }
    }
}

impl TaskStore {
    pub fn new(path: impl AsRef<Path>, size: usize) -> Result<Self> {
        let store = Arc::new(Store::new(path, size)?);
        let pending_queue = Arc::default();
        Ok(Self {
            store,
            pending_queue,
        })
    }

    pub async fn register(&self, index_uid: IndexUid, content: TaskContent) -> Result<Task> {
        debug!("registering update: {:?}", content);
        let store = self.store.clone();
        let task = tokio::task::spawn_blocking(move || -> Result<Task> {
            let mut txn = store.wtxn()?;
            let next_task_id = store.next_task_id(&mut txn)?;
            let created_at = TaskEvent::Created(Utc::now());
            let task = Task {
                id: next_task_id,
                index_uid,
                content,
                events: vec![created_at],
            };

            store.put(&mut txn, &task)?;

            txn.commit()?;

            Ok(task)
        })
        .await??;

        self.pending_queue.write().await.push(Reverse(task.id));

        Ok(task)
    }

    /// Returns the next task to process.
    pub async fn peek_pending(&self) -> Option<TaskId> {
        self.pending_queue.read().await.peek().map(|rid| rid.0)
    }

    /// Returns the next task to process if there is one.
    pub async fn get_processing_task(&self) -> Result<Option<Task>> {
        if let Some(uid) = self.peek_pending().await {
            let task = self.get_task(uid, None).await?;
            Ok(matches!(task.events.last(), Some(TaskEvent::Processing(_))).then(|| task))
        } else {
            Ok(None)
        }
    }

    pub async fn get_task(&self, id: TaskId, filter: Option<TaskFilter>) -> Result<Task> {
        let store = self.store.clone();
        let task = tokio::task::spawn_blocking(move || -> Result<_> {
            let txn = store.rtxn()?;
            let task = store.get(&txn, id)?;
            Ok(task)
        })
        .await??
        .ok_or(TaskError::UnexistingTask(id))?;

        match filter {
            Some(filter) => filter
                .pass(&task)
                .then(|| task)
                .ok_or(TaskError::UnexistingTask(id)),
            None => Ok(task),
        }
    }

    pub async fn update_tasks(&self, tasks: Vec<Task>) -> Result<()> {
        let store = self.store.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut txn = store.wtxn()?;

            for task in tasks {
                store.put(&mut txn, &task)?;
            }

            txn.commit()?;

            Ok(())
        })
        .await??;

        Ok(())
    }

    pub async fn delete_tasks(&self, to_remove: HashSet<TaskId>) -> Result<()> {
        let mut pending_queue = self.pending_queue.write().await;

        // currently retain is not stable: https://doc.rust-lang.org/stable/std/collections/struct.BinaryHeap.html#method.retain
        // pending_queue.retain(|id| !to_remove.contains(&id.0));
        *pending_queue = pending_queue
            .drain()
            .filter(|id| !to_remove.contains(&id.0))
            .collect();
        Ok(())
    }

    pub async fn list_tasks(
        &self,
        offset: Option<TaskId>,
        filter: Option<TaskFilter>,
        limit: Option<usize>,
    ) -> Result<Vec<Task>> {
        let store = self.store.clone();

        tokio::task::spawn_blocking(move || {
            let txn = store.rtxn()?;
            let tasks = store.list_tasks(&txn, offset, filter, limit)?;
            Ok(tasks)
        })
        .await?
    }
}

#[cfg(test)]
pub mod test {
    use super::*;

    use nelson::Mocker;
    use proptest::{
        strategy::Strategy,
        test_runner::{Config, TestRunner},
    };
    use tempfile::tempdir;

    pub enum MockTaskStore {
        Real(TaskStore),
        Mock(Arc<Mocker>),
    }

    impl Clone for MockTaskStore {
        fn clone(&self) -> Self {
            match self {
                Self::Real(x) => Self::Real(x.clone()),
                Self::Mock(x) => Self::Mock(x.clone()),
            }
        }
    }

    impl MockTaskStore {
        pub fn new(path: impl AsRef<Path>, size: usize) -> Result<Self> {
            Ok(Self::Real(TaskStore::new(path, size)?))
        }

        pub fn mock(mocker: Mocker) -> Self {
            Self::Mock(Arc::new(mocker))
        }

        pub async fn update_tasks(&self, tasks: Vec<Task>) -> Result<()> {
            match self {
                Self::Real(s) => s.update_tasks(tasks).await,
                Self::Mock(m) => unsafe { m.get::<_, Result<()>>("update_tasks").call(tasks) },
            }
        }

        pub async fn delete_tasks(&self, to_delete: HashSet<TaskId>) -> Result<()> {
            match self {
                Self::Real(s) => s.delete_tasks(to_delete).await,
                Self::Mock(m) => unsafe { m.get::<_, Result<()>>("delete_tasks").call(to_delete) },
            }
        }

        pub async fn get_task(&self, id: TaskId, filter: Option<TaskFilter>) -> Result<Task> {
            match self {
                Self::Real(s) => s.get_task(id, filter).await,
                Self::Mock(m) => unsafe { m.get::<_, Result<Task>>("get_task").call((id, filter)) },
            }
        }

        pub async fn get_processing_task(&self) -> Result<Option<Task>> {
            match self {
                Self::Real(s) => s.get_processing_task().await,
                Self::Mock(m) => unsafe {
                    m.get::<_, Result<Option<Task>>>("get_pending_task")
                        .call(())
                },
            }
        }

        pub async fn peek_pending(&self) -> Option<TaskId> {
            match self {
                Self::Real(s) => s.peek_pending().await,
                Self::Mock(m) => unsafe { m.get::<_, Option<TaskId>>("peek_pending").call(()) },
            }
        }

        pub async fn list_tasks(
            &self,
            from: Option<TaskId>,
            filter: Option<TaskFilter>,
            limit: Option<usize>,
        ) -> Result<Vec<Task>> {
            match self {
                Self::Real(s) => s.list_tasks(from, filter, limit).await,
                Self::Mock(_m) => todo!(),
            }
        }

        pub async fn register(&self, index_uid: IndexUid, content: TaskContent) -> Result<Task> {
            match self {
                Self::Real(s) => s.register(index_uid, content).await,
                Self::Mock(_m) => todo!(),
            }
        }
    }

    #[test]
    fn test_increment_task_id() {
        let mut runner = TestRunner::new(Config::default());

        let temp_dir = tempdir().unwrap();
        let store = Store::new(temp_dir.path(), 4096 * 1000).unwrap();

        let mut txn = store.wtxn().unwrap();
        assert_eq!(store.next_task_id(&mut txn).unwrap(), 0);
        txn.abort().unwrap();

        let gen_task = |id: TaskId| Task {
            id,
            index_uid: IndexUid::new_unchecked("test"),
            content: TaskContent::CreateIndex { primary_key: None },
            events: Vec::new(),
        };

        runner
            .run(&(0..100u64).prop_map(gen_task), |task| {
                let mut txn = store.wtxn().unwrap();
                let previous_id = store.next_task_id(&mut txn).unwrap();

                store.put(&mut txn, &task).unwrap();

                let next_id = store.next_task_id(&mut txn).unwrap();

                // if we put a task whose is is less that the next_id, then the next_id remains
                // unchanged, otherwise it becomes task.id + 1
                if task.id <= previous_id {
                    assert_eq!(next_id, previous_id)
                } else {
                    assert_eq!(next_id, task.id + 1);
                }

                txn.commit().unwrap();

                Ok(())
            })
            .unwrap();
    }
}
