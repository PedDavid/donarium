//! Read access to projects. The real implementation (CRDs plus the scan
//! results in the database) arrives with the next milestones; the in-memory
//! one backs `--demo` and the tests.

use crate::model::Project;

pub trait Repository: Send + Sync + std::fmt::Debug {
    /// All known projects, ordered by name.
    fn list(&self) -> Vec<Project>;

    fn get(&self, name: &str) -> Option<Project> {
        self.list().into_iter().find(|p| p.name == name)
    }

    /// True once the data can be served (a cache is synced, a database is open).
    fn ready(&self) -> bool;
}

#[derive(Debug, Default)]
pub struct MemoryRepository {
    projects: Vec<Project>,
}

impl MemoryRepository {
    pub fn new(mut projects: Vec<Project>) -> Self {
        projects.sort_by(|a, b| a.name.cmp(&b.name));
        MemoryRepository { projects }
    }
}

impl Repository for MemoryRepository {
    fn list(&self) -> Vec<Project> {
        self.projects.clone()
    }

    fn ready(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str) -> Project {
        Project {
            name: name.into(),
            repo: format!("github.com/example/{name}"),
            usage_score: 1.0,
            last_donation: None,
            funding: vec![],
        }
    }

    #[test]
    fn lists_sorted_and_finds_by_name() {
        let repo = MemoryRepository::new(vec![project("b"), project("a")]);
        let names: Vec<_> = repo.list().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["a", "b"]);
        assert!(repo.get("b").is_some());
        assert!(repo.get("c").is_none());
        assert!(repo.ready());
    }
}
