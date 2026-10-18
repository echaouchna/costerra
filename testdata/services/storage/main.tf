provider "google" {
  project = "example-platform"
  region  = "europe-west1"
}

locals {
  prod = { env = "prod", team = "data" }
  dev  = { env = "dev", team = "data" }
}

# ---- Cloud Storage -----------------------------------------------------------

# Prod regional bucket with lifecycle and versioning cleanup: no findings expected.
resource "google_storage_bucket" "prod_assets" {
  name                        = "example-prod-assets"
  location                    = "EUROPE-WEST1"
  uniform_bucket_level_access = true
  labels                      = local.prod

  versioning {
    enabled = true
  }

  lifecycle_rule {
    action {
      type          = "SetStorageClass"
      storage_class = "NEARLINE"
    }
    condition {
      age = 30
    }
  }

  lifecycle_rule {
    action {
      type = "Delete"
    }
    condition {
      num_newer_versions = 3
    }
  }
}

# Prod EU multi-region data lake without lifecycle rules (usage provided).
resource "google_storage_bucket" "prod_datalake" {
  name     = "example-prod-datalake"
  location = "eu"
  labels   = local.prod

  soft_delete_policy {
    retention_duration_seconds = 7776000 # 90 days
  }
}

# Dev multi-region bucket, versioned without noncurrent cleanup (usage provided).
resource "google_storage_bucket" "dev_uploads" {
  name     = "example-dev-uploads"
  location = "US"
  labels   = local.dev

  versioning {
    enabled = true
  }
}

# Dev configurable dual-region with turbo replication.
resource "google_storage_bucket" "dev_replicated" {
  name     = "example-dev-replicated"
  location = "EU"
  rpo      = "ASYNC_TURBO"
  labels   = local.dev

  custom_placement_config {
    data_locations = ["EUROPE-WEST1", "EUROPE-WEST4"]
  }
}

# Predefined dual-region archive bucket (prod).
resource "google_storage_bucket" "prod_archive" {
  name          = "example-prod-archive"
  location      = "EUR4"
  storage_class = "ARCHIVE"
  labels        = local.prod

  lifecycle_rule {
    action {
      type = "Delete"
    }
    condition {
      age = 2555
    }
  }
}

# Autoclass bucket with hierarchical namespace.
resource "google_storage_bucket" "prod_ml_datasets" {
  name     = "example-prod-ml-datasets"
  location = "US-CENTRAL1"
  labels   = local.prod

  autoclass {
    enabled                = true
    terminal_storage_class = "ARCHIVE"
  }

  hierarchical_namespace {
    enabled = true
  }

  uniform_bucket_level_access = true
}

# Dev scratch bucket: short-lived objects, soft delete left at its default.
resource "google_storage_bucket" "dev_scratch" {
  name          = "example-dev-scratch"
  location      = "europe-west1"
  storage_class = "NEARLINE"
  labels        = local.dev

  lifecycle_rule {
    action {
      type = "Delete"
    }
    condition {
      age = 3
    }
  }
}

# Dev multi-region bucket without usage estimates: unit-price comparison only.
resource "google_storage_bucket" "dev_static" {
  name     = "example-dev-static"
  location = "EU"
  labels   = local.dev

  lifecycle_rule {
    action {
      type = "Delete"
    }
    condition {
      age = 90
    }
  }
}

# Bucket without usage estimates: Autoclass suggestion.
resource "google_storage_bucket" "logs" {
  name     = "example-access-logs"
  location = "europe-west1"
}

resource "google_storage_anywhere_cache" "ml" {
  bucket = google_storage_bucket.prod_ml_datasets.name
  zone   = "us-central1-a"
  ttl    = "86400s"
}

# ---- Filestore ---------------------------------------------------------------

resource "google_filestore_instance" "prod_shared" {
  name     = "prod-shared"
  location = "europe-west1"
  tier     = "REGIONAL"
  labels   = local.prod

  file_shares {
    name        = "share1"
    capacity_gb = 2048
  }

  networks {
    network = "default"
    modes   = ["MODE_IPV4"]
  }
}

resource "google_filestore_instance" "dev_enterprise" {
  name     = "dev-enterprise"
  location = "europe-west1"
  tier     = "ENTERPRISE"
  labels   = local.dev

  file_shares {
    name        = "share1"
    capacity_gb = 1024
  }

  networks {
    network = "default"
    modes   = ["MODE_IPV4"]
  }
}

resource "google_filestore_instance" "dev_zonal_perf" {
  name     = "dev-zonal-perf"
  location = "europe-west1-b"
  tier     = "ZONAL"
  labels   = local.dev

  file_shares {
    name        = "share1"
    capacity_gb = 4096
  }

  performance_config {
    fixed_iops {
      max_iops = 17000
    }
  }

  networks {
    network = "default"
    modes   = ["MODE_IPV4"]
  }
}

resource "google_filestore_instance" "prod_home" {
  name     = "prod-home"
  location = "europe-west1-b"
  tier     = "BASIC_HDD"
  labels   = local.prod

  file_shares {
    name        = "home"
    capacity_gb = 8192
  }

  networks {
    network = "default"
    modes   = ["MODE_IPV4"]
  }
}

resource "google_filestore_instance" "legacy_premium" {
  name     = "legacy-premium"
  location = "europe-west1-c"
  tier     = "PREMIUM"
  labels   = local.prod

  file_shares {
    name        = "vol1"
    capacity_gb = 2560
  }

  networks {
    network = "default"
    modes   = ["MODE_IPV4"]
  }
}

resource "google_filestore_backup" "prod_home" {
  name              = "prod-home-backup"
  location          = "europe-west1"
  source_instance   = google_filestore_instance.prod_home.id
  source_file_share = "home"
}

resource "google_filestore_snapshot" "prod_shared" {
  name     = "prod-shared-snap"
  instance = google_filestore_instance.prod_shared.name
  location = "europe-west1"
}

# ---- Artifact Registry --------------------------------------------------------

resource "google_artifact_registry_repository" "docker" {
  repository_id = "apps"
  location      = "europe-west1"
  format        = "DOCKER"
  labels        = local.prod
}

resource "google_artifact_registry_repository" "docker_clean" {
  repository_id = "apps-clean"
  location      = "europe"
  format        = "DOCKER"

  cleanup_policies {
    id     = "keep-recent"
    action = "KEEP"
    most_recent_versions {
      keep_count = 10
    }
  }

  cleanup_policies {
    id     = "delete-untagged"
    action = "DELETE"
    condition {
      tag_state  = "UNTAGGED"
      older_than = "2592000s"
    }
  }
}

resource "google_artifact_registry_repository" "dockerhub_cache" {
  repository_id          = "dockerhub"
  location               = "europe-west1"
  format                 = "DOCKER"
  mode                   = "REMOTE_REPOSITORY"
  cleanup_policy_dry_run = true

  remote_repository_config {
    docker_repository {
      public_repository = "DOCKER_HUB"
    }
  }

  cleanup_policies {
    id     = "delete-old"
    action = "DELETE"
    condition {
      older_than = "604800s"
    }
  }
}

resource "google_artifact_registry_repository" "virtual" {
  repository_id = "all-docker"
  location      = "europe-west1"
  format        = "DOCKER"
  mode          = "VIRTUAL_REPOSITORY"

  virtual_repository_config {
    upstream_policies {
      id         = "apps"
      repository = google_artifact_registry_repository.docker.id
      priority   = 1
    }
  }
}

# ---- Backup and DR -------------------------------------------------------------

resource "google_compute_instance" "app" {
  name         = "prod-app"
  machine_type = "e2-standard-2"
  zone         = "europe-west1-b"
  labels       = local.prod

  boot_disk {
    initialize_params {
      image = "debian-cloud/debian-12"
      size  = 50
      type  = "pd-balanced"
    }
  }

  attached_disk {
    source = google_compute_disk.app_data.id
  }

  network_interface {
    network = "default"
  }
}

resource "google_compute_disk" "app_data" {
  name   = "prod-app-data"
  type   = "pd-balanced"
  size   = 200
  zone   = "europe-west1-b"
  labels = local.prod
}

resource "google_backup_dr_backup_vault" "prod" {
  backup_vault_id                            = "prod-vault"
  location                                   = "europe-west1"
  backup_minimum_enforced_retention_duration = "2592000s"
  labels                                     = local.prod
}

resource "google_backup_dr_backup_vault" "dev" {
  backup_vault_id                            = "dev-vault"
  location                                   = "europe-west1"
  backup_minimum_enforced_retention_duration = "7776000s"
  labels                                     = local.dev
}

resource "google_backup_dr_backup_plan" "daily" {
  backup_plan_id = "daily"
  location       = "europe-west1"
  resource_type  = "compute.googleapis.com/Instance"
  backup_vault   = google_backup_dr_backup_vault.prod.id

  backup_rules {
    rule_id               = "daily"
    backup_retention_days = 30
    standard_schedule {
      recurrence_type  = "DAILY"
      hourly_frequency = 0
      time_zone        = "UTC"
      backup_window {
        start_hour_of_day = 1
        end_hour_of_day   = 5
      }
    }
  }
}

resource "google_backup_dr_backup_plan_association" "app" {
  location                   = "europe-west1"
  backup_plan_association_id = "prod-app"
  resource                   = google_compute_instance.app.id
  resource_type              = "compute.googleapis.com/Instance"
  backup_plan                = google_backup_dr_backup_plan.daily.name
}

resource "google_storage_transfer_job" "nightly_copy" {
  description = "Nightly copy of assets to the archive bucket"

  transfer_spec {
    gcs_data_source {
      bucket_name = google_storage_bucket.prod_assets.name
    }
    gcs_data_sink {
      bucket_name = google_storage_bucket.prod_archive.name
    }
  }

  schedule {
    schedule_start_date {
      year  = 2026
      month = 1
      day   = 1
    }
  }
}
