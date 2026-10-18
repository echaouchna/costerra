variable "db_password" {
  type      = string
  sensitive = true
}

provider "google" {
  project = "example-data"
  region  = "europe-west1"
}

locals {
  prod = { env = "prod", team = "data" }
  dev  = { env = "dev", team = "data" }
}

# ---------------------------------------------------------------- Cloud SQL

resource "google_sql_database_instance" "prod_postgres" {
  name             = "prod-postgres"
  database_version = "POSTGRES_15"

  settings {
    tier              = "db-custom-4-16384"
    availability_type = "REGIONAL"
    disk_size         = 200
    user_labels       = local.prod

    backup_configuration {
      enabled                        = true
      point_in_time_recovery_enabled = true
    }
  }
}

resource "google_sql_database_instance" "prod_postgres_replica" {
  name                 = "prod-postgres-replica"
  database_version     = "POSTGRES_15"
  master_instance_name = google_sql_database_instance.prod_postgres.name

  settings {
    tier        = "db-custom-4-16384"
    disk_size   = 200
    user_labels = local.prod
  }
}

# POSTGRES_16 without an explicit edition (defaults to ENTERPRISE_PLUS on new instances).
resource "google_sql_database_instance" "dev_postgres" {
  name             = "dev-postgres"
  database_version = "POSTGRES_16"

  settings {
    tier              = "db-custom-2-7680"
    availability_type = "REGIONAL"
    disk_size         = 100
    user_labels       = local.dev
  }
}

resource "google_sql_database_instance" "dev_mysql_legacy" {
  name             = "dev-mysql-legacy"
  region           = "europe-west4"
  database_version = "MYSQL_5_7"

  settings {
    tier      = "db-n1-standard-2"
    disk_size = 50
  }
}

resource "google_sql_database_instance" "staging_enterprise_plus" {
  name             = "staging-analytics"
  database_version = "POSTGRES_16"

  settings {
    edition   = "ENTERPRISE_PLUS"
    tier      = "db-perf-optimized-N-4"
    disk_size = 250

    data_cache_config {
      data_cache_enabled = true
    }
  }
}

resource "google_sql_database_instance" "prod_sqlserver" {
  name             = "prod-erp"
  database_version = "SQLSERVER_2022_STANDARD"
  root_password    = var.db_password

  settings {
    tier              = "db-custom-8-32768"
    availability_type = "REGIONAL"
    disk_size         = 500
  }
}

resource "google_sql_database_instance" "dev_sqlserver_express" {
  name             = "dev-reporting"
  database_version = "SQLSERVER_2019_EXPRESS"
  root_password    = var.db_password

  settings {
    tier      = "db-custom-2-7680"
    disk_size = 20
  }
}

resource "google_sql_database_instance" "test_micro" {
  name             = "test-sandbox"
  database_version = "POSTGRES_15"

  settings {
    tier      = "db-f1-micro"
    disk_type = "PD_HDD"
  }
}

resource "google_sql_database_instance" "prod_c4a" {
  name             = "prod-orders"
  database_version = "POSTGRES_17"

  settings {
    edition                          = "ENTERPRISE_PLUS"
    tier                             = "db-c4a-highmem-8"
    disk_type                        = "HYPERDISK_BALANCED"
    disk_size                        = 500
    data_disk_provisioned_iops       = 10000
    data_disk_provisioned_throughput = 400
  }
}

resource "google_sql_database_instance" "dev_mysql_stopped" {
  name             = "dev-mysql-stopped"
  database_version = "MYSQL_8_0"

  settings {
    tier              = "db-custom-1-3840"
    activation_policy = "NEVER"
  }
}

resource "google_sql_database_instance" "prod_archive" {
  name             = "prod-archive"
  database_version = "MYSQL_8_0"

  settings {
    tier            = "db-custom-N4-4-16384"
    disk_size       = 2000
    disk_autoresize = false
  }
}

resource "google_sql_database" "app" {
  name     = "app"
  instance = google_sql_database_instance.prod_postgres.name
}

resource "google_sql_user" "app" {
  name     = "app"
  instance = google_sql_database_instance.prod_postgres.name
  password = var.db_password
}

# ---------------------------------------------------------------- AlloyDB

resource "google_alloydb_cluster" "prod" {
  cluster_id = "prod-alloydb"
  location   = "europe-west1"
  labels     = local.prod

  network_config {
    network = "projects/example-data/global/networks/default"
  }
}

resource "google_alloydb_instance" "prod_primary" {
  cluster       = google_alloydb_cluster.prod.name
  instance_id   = "prod-primary"
  instance_type = "PRIMARY"
  labels        = local.prod

  machine_config {
    cpu_count = 8
  }
}

resource "google_alloydb_instance" "prod_read_pool" {
  cluster       = google_alloydb_cluster.prod.name
  instance_id   = "prod-read-pool"
  instance_type = "READ_POOL"
  labels        = local.prod

  read_pool_config {
    node_count = 2
  }

  machine_config {
    cpu_count = 4
  }
}

resource "google_alloydb_cluster" "dev" {
  cluster_id = "dev-alloydb"
  location   = "europe-west1"
  labels     = local.dev

  continuous_backup_config {
    enabled = false
  }
}

resource "google_alloydb_instance" "dev_primary" {
  cluster       = google_alloydb_cluster.dev.name
  instance_id   = "dev-primary"
  instance_type = "PRIMARY"
  labels        = local.dev

  machine_config {
    cpu_count = 2
  }
}

resource "google_alloydb_instance" "dev_read_pool" {
  cluster       = google_alloydb_cluster.dev.name
  instance_id   = "dev-read-pool"
  instance_type = "READ_POOL"
  labels        = local.dev

  read_pool_config {
    node_count = 1
  }

  machine_config {
    cpu_count = 2
  }
}

# ---------------------------------------------------------------- Spanner

resource "google_spanner_instance" "prod_global" {
  name         = "prod-global"
  config       = "eur3"
  display_name = "Prod global"
  edition      = "ENTERPRISE_PLUS"
  num_nodes    = 3
  labels       = local.prod
}

resource "google_spanner_instance" "prod_autoscaled" {
  name         = "prod-catalog"
  config       = "regional-europe-west1"
  display_name = "Prod catalog"
  edition      = "ENTERPRISE"
  labels       = local.prod

  autoscaling_config {
    autoscaling_limits {
      min_nodes = 1
      max_nodes = 5
    }
    autoscaling_targets {
      high_priority_cpu_utilization_percent = 65
      storage_utilization_percent           = 90
    }
  }
}

resource "google_spanner_instance" "dev" {
  name         = "dev-spanner"
  config       = "regional-europe-west1"
  display_name = "Dev"
  edition      = "ENTERPRISE"
  num_nodes    = 1
  labels       = local.dev
}

resource "google_spanner_instance" "staging_multi" {
  name             = "staging-spanner"
  config           = "nam3"
  display_name     = "Staging"
  processing_units = 1000
  labels           = { env = "staging" }
}

resource "google_spanner_database" "catalog" {
  instance = google_spanner_instance.prod_autoscaled.name
  name     = "catalog"
}

# ---------------------------------------------------------------- Bigtable

resource "google_bigtable_instance" "prod" {
  name   = "prod-events"
  labels = local.prod

  cluster {
    cluster_id   = "prod-events-c1"
    zone         = "europe-west1-b"
    num_nodes    = 3
    storage_type = "SSD"
  }

  cluster {
    cluster_id = "prod-events-c2"
    zone       = "europe-west4-a"

    autoscaling_config {
      min_nodes  = 2
      max_nodes  = 6
      cpu_target = 60
    }
  }
}

resource "google_bigtable_instance" "dev" {
  name   = "dev-events"
  labels = local.dev

  cluster {
    cluster_id = "dev-events-c1"
    zone       = "europe-west1-b"
    num_nodes  = 3
  }

  cluster {
    cluster_id = "dev-events-c2"
    zone       = "europe-west1-c"
    num_nodes  = 3
  }
}

resource "google_bigtable_instance" "prod_archive" {
  name   = "prod-archive"
  labels = local.prod

  cluster {
    cluster_id   = "prod-archive-c1"
    zone         = "europe-west1-d"
    num_nodes    = 1
    storage_type = "HDD"
  }
}

resource "google_bigtable_table" "events" {
  name          = "events"
  instance_name = google_bigtable_instance.prod.name
}

# ---------------------------------------------------------------- Memorystore

resource "google_redis_instance" "prod_cache" {
  name               = "prod-cache"
  tier               = "STANDARD_HA"
  memory_size_gb     = 10
  read_replicas_mode = "READ_REPLICAS_ENABLED"
  replica_count      = 2
  labels             = local.prod
}

resource "google_redis_instance" "dev_cache" {
  name           = "dev-cache"
  tier           = "STANDARD_HA"
  memory_size_gb = 5
  labels         = local.dev
}

resource "google_redis_instance" "dev_sessions" {
  name           = "dev-sessions"
  memory_size_gb = 1
  labels         = local.dev
}

resource "google_redis_cluster" "prod" {
  name          = "prod-redis-cluster"
  region        = "europe-west1"
  shard_count   = 3
  replica_count = 1

  persistence_config {
    mode = "AOF"
  }

  psc_configs {
    network = "projects/example-data/global/networks/default"
  }
}

resource "google_redis_cluster" "dev" {
  name          = "dev-redis-cluster"
  region        = "europe-west1"
  shard_count   = 1
  replica_count = 1
  node_type     = "REDIS_SHARED_CORE_NANO"

  psc_configs {
    network = "projects/example-data/global/networks/default"
  }
}

resource "google_memorystore_instance" "prod_valkey" {
  instance_id   = "prod-valkey"
  location      = "europe-west1"
  shard_count   = 3
  replica_count = 1
  node_type     = "HIGHMEM_MEDIUM"
  labels        = local.prod
}

resource "google_memorystore_instance" "dev_valkey" {
  instance_id   = "dev-valkey"
  location      = "europe-west1"
  shard_count   = 1
  replica_count = 1
  mode          = "CLUSTER_DISABLED"
  node_type     = "SHARED_CORE_NANO"
  labels        = local.dev
}

resource "google_memcache_instance" "prod" {
  name       = "prod-memcache"
  node_count = 3
  labels     = local.prod

  node_config {
    cpu_count      = 4
    memory_size_mb = 8192
  }
}

resource "google_memcache_instance" "dev" {
  name       = "dev-memcache"
  node_count = 1
  labels     = local.dev

  node_config {
    cpu_count      = 1
    memory_size_mb = 1024
  }
}

# ---------------------------------------------------------------- Firestore

resource "google_firestore_database" "default" {
  name        = "(default)"
  location_id = "nam5"
  type        = "FIRESTORE_NATIVE"
}

resource "google_firestore_database" "dev" {
  name                              = "dev-db"
  location_id                       = "europe-west1"
  type                              = "FIRESTORE_NATIVE"
  point_in_time_recovery_enablement = "POINT_IN_TIME_RECOVERY_ENABLED"
}

resource "google_firestore_database" "prod_enterprise" {
  name             = "prod-docs"
  location_id      = "eur3"
  type             = "FIRESTORE_NATIVE"
  database_edition = "ENTERPRISE"
}
