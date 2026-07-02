use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::dataset::{Dataset, HostVars};

// CacheEntry envuelve un Dataset con metadata de caché a tres niveles:
// - dataset level: cuándo se hizo el último sync completo
// - host level: cuándo se refrescó cada host individual
// - group level: se resuelve consultando los hosts del grupo
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub dataset: Dataset,
    pub fetched_at: Instant,
    pub ttl: Duration,
    // Timestamp individual por host — permite saber cuándo se refrescó cada uno
    // Si un host no está aquí, se usa fetched_at del dataset
    pub host_timestamps: HashMap<String, Instant>,
}

impl CacheEntry {
    pub fn new(dataset: Dataset, ttl_seconds: u64) -> Self {
        let now = Instant::now();

        // Al crear, todos los hosts tienen el mismo timestamp
        let host_timestamps: HashMap<String, Instant> = dataset
            .hostvars
            .keys()
            .map(|host| (host.clone(), now))
            .collect();

        Self {
            dataset,
            fetched_at: now,
            ttl: Duration::from_secs(ttl_seconds),
            host_timestamps,
        }
    }

    // ¿El dataset completo está fresco?
    pub fn is_fresh(&self) -> bool {
        self.fetched_at.elapsed() < self.ttl
    }

    // Edad del dataset completo en segundos
    pub fn age_seconds(&self) -> u64 {
        self.fetched_at.elapsed().as_secs()
    }

    // ¿Un host específico está fresco? Acepta un TTL override por host
    pub fn is_host_fresh(&self, hostname: &str, ttl_override: Option<u64>) -> bool {
        let ttl = match ttl_override {
            Some(secs) => Duration::from_secs(secs),
            None => self.ttl, // si no hay override, usa el TTL global
        };

        match self.host_timestamps.get(hostname) {
            Some(timestamp) => timestamp.elapsed() < ttl,
            None => false, // host no existe en cache
        }
    }

    // Edad de un host específico en segundos
    pub fn host_age_seconds(&self, hostname: &str) -> Option<u64> {
        self.host_timestamps
            .get(hostname)
            .map(|ts| ts.elapsed().as_secs())
    }

    // Actualiza un solo host: sus vars y su timestamp
    // &mut self = referencia MUTABLE — podemos modificar la instancia
    // Es la primera vez que vemos &mut: hasta ahora todo era &self (solo lectura)
    pub fn update_host(&mut self, hostname: String, vars: HostVars) {
        self.dataset.hostvars.insert(hostname.clone(), vars);
        self.host_timestamps.insert(hostname, Instant::now());
    }

    // Merge: parchea los hosts que vienen, el resto no se toca.
    // También procesa remove_hosts si vienen.
    pub fn merge_dataset(&mut self, partial: Dataset) {
        let now = Instant::now();

        // Merge hostvars
        for (hostname, vars) in partial.hostvars {
            self.dataset.hostvars.insert(hostname.clone(), vars);
            self.host_timestamps.insert(hostname, now);
        }

        // Merge groups
        for (group_name, group) in partial.groups {
            self.dataset.groups.insert(group_name, group);
        }

        // Remove hosts
        for hostname in &partial.remove_hosts {
            self.dataset.hostvars.remove(hostname);
            self.host_timestamps.remove(hostname);
            // Quitar el host de todos los grupos
            for group in self.dataset.groups.values_mut() {
                group.hosts.retain(|h| h != hostname);
            }
        }
    }

    // Elimina un host del cache
    pub fn remove_host(&mut self, hostname: &str) {
        self.dataset.hostvars.remove(hostname);
        self.host_timestamps.remove(hostname);
        for group in self.dataset.groups.values_mut() {
            group.hosts.retain(|h| h != hostname);
        }
    }

    pub fn update_group(&mut self, group_name: &str, partial_dataset: Dataset) {
        // Solo actualizamos los hosts que pertenecen al grupo
        if let Some(group) = self.dataset.groups.get(group_name) {
            let now = Instant::now();
            for hostname in &group.hosts {
                if let Some(vars) = partial_dataset.hostvars.get(hostname) {
                    self.dataset.hostvars.insert(hostname.clone(), vars.clone());
                    self.host_timestamps.insert(hostname.clone(), now);
                }
            }
        }

        // Actualizamos las vars del grupo si vienen
        if let Some(new_group) = partial_dataset.groups.get(group_name) {
            if let Some(existing_group) = self.dataset.groups.get_mut(group_name) {
                existing_group.vars = new_group.vars.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn empty_dataset() -> Dataset {
        Dataset {
            hostvars: HashMap::new(),
            groups: HashMap::new(),
            remove_hosts: vec![],
        }
    }

    fn dataset_with_hosts() -> Dataset {
        let mut hostvars = HashMap::new();
        hostvars.insert(
            "motoko.section9.net".to_string(),
            [("role".to_string(), serde_json::json!("commander"))]
                .into_iter()
                .collect(),
        );
        hostvars.insert(
            "batou.section9.net".to_string(),
            [("role".to_string(), serde_json::json!("assault"))]
                .into_iter()
                .collect(),
        );
        Dataset {
            hostvars,
            groups: HashMap::new(),
            remove_hosts: vec![],
        }
    }

    #[test]
    fn new_entry_is_fresh() {
        let entry = CacheEntry::new(empty_dataset(), 3600);
        assert!(entry.is_fresh());
    }

    #[test]
    fn expired_entry_is_not_fresh() {
        let entry = CacheEntry::new(empty_dataset(), 0);
        thread::sleep(std::time::Duration::from_millis(10));
        assert!(!entry.is_fresh());
    }

    #[test]
    fn age_starts_at_zero() {
        let entry = CacheEntry::new(empty_dataset(), 3600);
        assert_eq!(entry.age_seconds(), 0);
    }

    #[test]
    fn ttl_is_set_correctly() {
        let entry = CacheEntry::new(empty_dataset(), 300);
        assert_eq!(entry.ttl, std::time::Duration::from_secs(300));
    }

    #[test]
    fn host_timestamps_created_on_new() {
        let entry = CacheEntry::new(dataset_with_hosts(), 3600);
        assert_eq!(entry.host_timestamps.len(), 2);
        assert!(entry.host_timestamps.contains_key("motoko.section9.net"));
    }

    #[test]
    fn host_is_fresh_with_global_ttl() {
        let entry = CacheEntry::new(dataset_with_hosts(), 3600);
        assert!(entry.is_host_fresh("motoko.section9.net", None));
    }

    #[test]
    fn host_is_stale_with_override_ttl() {
        let entry = CacheEntry::new(dataset_with_hosts(), 3600);
        thread::sleep(std::time::Duration::from_millis(10));
        // Override de 0 segundos = siempre expirado
        assert!(!entry.is_host_fresh("motoko.section9.net", Some(0)));
    }

    #[test]
    fn unknown_host_is_not_fresh() {
        let entry = CacheEntry::new(dataset_with_hosts(), 3600);
        assert!(!entry.is_host_fresh("togusa.section9.net", None));
    }

    #[test]
    fn update_host_refreshes_timestamp() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);
        let original_ts = entry.host_timestamps["motoko.section9.net"];

        thread::sleep(std::time::Duration::from_millis(10));

        let new_vars = [("role".to_string(), serde_json::json!("upgraded"))]
            .into_iter()
            .collect();
        entry.update_host("motoko.section9.net".to_string(), new_vars);

        // El timestamp debe haber cambiado
        assert!(entry.host_timestamps["motoko.section9.net"] > original_ts);
        // Los vars deben estar actualizados
        assert_eq!(entry.dataset.hostvars["motoko.section9.net"]["role"], "upgraded");
    }

    #[test]
    fn host_age_returns_none_for_unknown() {
        let entry = CacheEntry::new(dataset_with_hosts(), 3600);
        assert!(entry.host_age_seconds("togusa.section9.net").is_none());
    }

    #[test]
    fn merge_dataset_adds_new_hosts() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);
        assert_eq!(entry.dataset.hostvars.len(), 2);

        let partial = Dataset {
            hostvars: [(
                "togusa.section9.net".to_string(),
                [("role".to_string(), serde_json::json!("detective"))].into_iter().collect(),
            )].into_iter().collect(),
            groups: HashMap::new(),
            remove_hosts: vec![],
        };

        entry.merge_dataset(partial);
        assert_eq!(entry.dataset.hostvars.len(), 3);
        assert_eq!(entry.dataset.hostvars["togusa.section9.net"]["role"], "detective");
    }

    #[test]
    fn merge_dataset_updates_existing_hosts() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);

        let partial = Dataset {
            hostvars: [(
                "motoko.section9.net".to_string(),
                [("role".to_string(), serde_json::json!("major"))].into_iter().collect(),
            )].into_iter().collect(),
            groups: HashMap::new(),
            remove_hosts: vec![],
        };

        entry.merge_dataset(partial);
        assert_eq!(entry.dataset.hostvars.len(), 2);
        assert_eq!(entry.dataset.hostvars["motoko.section9.net"]["role"], "major");
    }

    #[test]
    fn merge_dataset_removes_hosts() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);
        assert_eq!(entry.dataset.hostvars.len(), 2);

        let partial = Dataset {
            hostvars: HashMap::new(),
            groups: HashMap::new(),
            remove_hosts: vec!["batou.section9.net".to_string()],
        };

        entry.merge_dataset(partial);
        assert_eq!(entry.dataset.hostvars.len(), 1);
        assert!(!entry.dataset.hostvars.contains_key("batou.section9.net"));
        assert!(!entry.host_timestamps.contains_key("batou.section9.net"));
    }

    #[test]
    fn remove_host_deletes_from_groups() {
        use crate::domain::dataset::Group;

        let mut entry = CacheEntry::new(Dataset {
            hostvars: [(
                "motoko.section9.net".to_string(),
                [("role".to_string(), serde_json::json!("commander"))].into_iter().collect(),
            )].into_iter().collect(),
            groups: [(
                "section9".to_string(),
                Group {
                    hosts: vec!["motoko.section9.net".to_string()],
                    children: vec![],
                    vars: None,
                },
            )].into_iter().collect(),
            remove_hosts: vec![],
        }, 3600);

        entry.remove_host("motoko.section9.net");
        assert!(!entry.dataset.hostvars.contains_key("motoko.section9.net"));
        assert!(entry.dataset.groups["section9"].hosts.is_empty());
    }
}
