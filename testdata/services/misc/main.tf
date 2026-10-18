provider "google" {
  project = "example-platform"
  region  = "europe-west1"
}

locals {
  prod = { env = "prod", team = "platform" }
  dev  = { env = "dev", team = "platform" }
}

# ---------------------------------------------------------------------------
# Cloud KMS
# ---------------------------------------------------------------------------

resource "google_kms_key_ring" "prod" {
  name     = "prod-keyring"
  location = "europe-west1"
}

resource "google_kms_key_ring" "dev" {
  name     = "dev-keyring"
  location = "europe"
}

resource "google_kms_key_ring" "global" {
  name     = "global-keyring"
  location = "global"
}

# Software symmetric CMEK key, rotated every 90 days.
resource "google_kms_crypto_key" "cmek" {
  name            = "cmek-storage"
  key_ring        = google_kms_key_ring.prod.id
  rotation_period = "7776000s"
  labels          = local.prod
}

# HSM symmetric key rotated weekly: versions pile up.
resource "google_kms_crypto_key" "hsm_payments" {
  name            = "payments-hsm"
  key_ring        = google_kms_key_ring.prod.id
  rotation_period = "604800s"
  labels          = local.prod

  version_template {
    algorithm        = "GOOGLE_SYMMETRIC_ENCRYPTION"
    protection_level = "HSM"
  }
}

# HSM signing key in dev.
resource "google_kms_crypto_key" "dev_signing" {
  name     = "dev-signing"
  key_ring = google_kms_key_ring.dev.id
  purpose  = "ASYMMETRIC_SIGN"
  labels   = local.dev

  version_template {
    algorithm        = "EC_SIGN_P384_SHA384"
    protection_level = "HSM"
  }
}

resource "google_kms_crypto_key" "webhook_mac" {
  name     = "webhook-mac"
  key_ring = google_kms_key_ring.global.id
  purpose  = "MAC"
  labels   = local.prod

  version_template {
    algorithm = "HMAC_SHA256"
  }
}

# Imported RSA key: versions are declared explicitly.
resource "google_kms_crypto_key" "imported" {
  name                          = "imported-rsa"
  key_ring                      = google_kms_key_ring.prod.id
  purpose                       = "ASYMMETRIC_DECRYPT"
  skip_initial_version_creation = true
  labels                        = local.prod

  version_template {
    algorithm        = "RSA_DECRYPT_OAEP_2048_SHA256"
    protection_level = "HSM"
  }
}

resource "google_kms_crypto_key_version" "imported_v1" {
  crypto_key = google_kms_crypto_key.imported.id
}

resource "google_kms_crypto_key_version" "imported_v2" {
  crypto_key = google_kms_crypto_key.imported.id
}

resource "google_kms_crypto_key" "ekm" {
  name     = "ekm-key"
  key_ring = google_kms_key_ring.prod.id
  labels   = local.prod

  version_template {
    algorithm        = "EXTERNAL_SYMMETRIC_ENCRYPTION"
    protection_level = "EXTERNAL_VPC"
  }
}

resource "google_kms_key_handle" "bq" {
  name                   = "bq-handle"
  location               = "europe-west1"
  resource_type_selector = "bigquery.googleapis.com/Dataset"
}

# ---------------------------------------------------------------------------
# Secret Manager
# ---------------------------------------------------------------------------

resource "google_secret_manager_secret" "db_password" {
  secret_id = "db-password"
  labels    = local.prod

  replication {
    auto {}
  }

  rotation {
    rotation_period    = "2592000s"
    next_rotation_time = "2026-10-01T00:00:00Z"
  }

  topics {
    name = "projects/example-platform/topics/secret-rotation"
  }
}

resource "google_secret_manager_secret_version" "db_password" {
  secret      = google_secret_manager_secret.db_password.id
  secret_data = "changeme"
}

resource "google_secret_manager_secret" "api_keys" {
  secret_id = "api-keys"
  labels    = local.prod

  replication {
    user_managed {
      replicas {
        location = "europe-west1"
      }
      replicas {
        location = "europe-west4"
      }
      replicas {
        location = "us-central1"
      }
      replicas {
        location = "asia-southeast1"
      }
    }
  }
}

resource "google_secret_manager_regional_secret" "dev_token" {
  secret_id = "dev-token"
  location  = "europe-west1"
  labels    = local.dev
}

resource "google_secret_manager_regional_secret_version" "dev_token" {
  secret      = google_secret_manager_regional_secret.dev_token.id
  secret_data = "dev"
}

# ---------------------------------------------------------------------------
# Cloud Logging
# ---------------------------------------------------------------------------

resource "google_logging_project_bucket_config" "default" {
  project        = "example-platform"
  location       = "global"
  bucket_id      = "_Default"
  retention_days = 30
}

resource "google_logging_project_bucket_config" "required" {
  project        = "example-platform"
  location       = "global"
  bucket_id      = "_Required"
  retention_days = 400
}

resource "google_logging_project_bucket_config" "audit_archive" {
  project          = "example-platform"
  location         = "europe-west1"
  bucket_id        = "audit-archive"
  retention_days   = 1095
  enable_analytics = true
}

resource "google_logging_linked_dataset" "audit_archive" {
  link_id  = "audit_archive"
  bucket   = google_logging_project_bucket_config.audit_archive.id
  location = "europe-west1"
}

resource "google_logging_project_bucket_config" "dev_app_logs" {
  project        = "example-dev"
  location       = "europe-west1"
  bucket_id      = "app-logs"
  retention_days = 90
}

resource "google_logging_folder_bucket_config" "shared" {
  folder         = "folders/123456789"
  location       = "global"
  bucket_id      = "_Default"
  retention_days = 60
}

resource "google_logging_project_sink" "to_archive" {
  name        = "audit-to-archive"
  destination = "logging.googleapis.com/${google_logging_project_bucket_config.audit_archive.id}"
  filter      = "logName:\"cloudaudit.googleapis.com\""
}

# ---------------------------------------------------------------------------
# Cloud Monitoring
# ---------------------------------------------------------------------------

resource "google_monitoring_uptime_check_config" "api" {
  display_name = "api-prod"
  timeout      = "10s"
  period       = "60s"
  user_labels  = local.prod

  http_check {
    path    = "/healthz"
    port    = 443
    use_ssl = true
  }

  monitored_resource {
    type = "uptime_url"
    labels = {
      project_id = "example-platform"
      host       = "api.example.example"
    }
  }
}

resource "google_monitoring_uptime_check_config" "dev_web" {
  display_name     = "web-dev"
  timeout          = "10s"
  period           = "60s"
  selected_regions = ["EUROPE", "USA_VIRGINIA", "ASIA_PACIFIC"]
  user_labels      = local.dev

  http_check {
    path = "/"
    port = 80
  }

  monitored_resource {
    type = "uptime_url"
    labels = {
      project_id = "example-dev"
      host       = "web.dev.example.example"
    }
  }
}

resource "google_monitoring_uptime_check_config" "checkout_journey" {
  display_name     = "checkout-journey"
  timeout          = "60s"
  period           = "300s"
  selected_regions = ["USA", "EUROPE"]
  user_labels      = local.prod

  synthetic_monitor {
    cloud_function_v2 {
      name = "projects/example-platform/locations/europe-west1/functions/checkout-synthetic"
    }
  }
}

resource "google_monitoring_metric_descriptor" "orders" {
  description  = "Orders processed"
  display_name = "orders"
  type         = "custom.googleapis.com/shop/orders"
  metric_kind  = "GAUGE"
  value_type   = "DOUBLE"

  metadata {
    sample_period = "30s"
  }
}

resource "google_monitoring_metric_descriptor" "latency" {
  description  = "Request latency"
  display_name = "latency"
  type         = "custom.googleapis.com/shop/latency"
  metric_kind  = "CUMULATIVE"
  value_type   = "DISTRIBUTION"
}

# ---------------------------------------------------------------------------
# Cloud Build
# ---------------------------------------------------------------------------

resource "google_cloudbuild_worker_pool" "ci" {
  name     = "ci-pool"
  location = "europe-west1"

  worker_config {
    machine_type   = "e2-standard-4"
    disk_size_gb   = 200
    no_external_ip = true
  }
}

resource "google_cloudbuild_worker_pool" "dev_heavy" {
  name     = "dev-heavy-pool"
  location = "europe-west1"

  worker_config {
    machine_type = "c3-standard-22"
  }
}

resource "google_cloudbuild_trigger" "app" {
  name     = "app-main"
  filename = "cloudbuild.yaml"

  github {
    owner = "example"
    name  = "app"
    push {
      branch = "^main$"
    }
  }
}

resource "google_cloudbuild_trigger" "images" {
  name     = "images"
  location = "europe-west1"

  trigger_template {
    repo_name   = "images"
    branch_name = "main"
  }

  build {
    step {
      name = "gcr.io/cloud-builders/docker"
      args = ["build", "-t", "europe-docker.pkg.dev/example/images/base", "."]
    }
    options {
      machine_type = "E2_HIGHCPU_8"
      disk_size_gb = 300
    }
  }
}

resource "google_cloudbuild_trigger" "private" {
  name     = "private-deploy"
  location = "europe-west1"

  trigger_template {
    repo_name   = "infra"
    branch_name = "main"
  }

  build {
    step {
      name = "hashicorp/terraform"
      args = ["apply", "-auto-approve"]
    }
    options {
      worker_pool = google_cloudbuild_worker_pool.ci.id
    }
  }
}

# ---------------------------------------------------------------------------
# Cloud Workstations
# ---------------------------------------------------------------------------

resource "google_workstations_workstation_cluster" "main" {
  workstation_cluster_id = "main"
  location               = "europe-west1"
  network                = "projects/example-platform/global/networks/dev"
  subnetwork             = "projects/example-platform/regions/europe-west1/subnetworks/dev"
  labels                 = local.prod
}

resource "google_workstations_workstation_cluster" "team_b" {
  workstation_cluster_id = "team-b"
  location               = "europe-west1"
  network                = "projects/example-platform/global/networks/dev"
  subnetwork             = "projects/example-platform/regions/europe-west1/subnetworks/dev"
  labels                 = local.prod
}

resource "google_workstations_workstation_config" "dev" {
  workstation_config_id  = "dev"
  workstation_cluster_id = google_workstations_workstation_cluster.main.workstation_cluster_id
  location               = "europe-west1"
  idle_timeout           = "0s"
  labels                 = local.dev

  host {
    gce_instance {
      machine_type      = "e2-standard-8"
      boot_disk_size_gb = 50
      pool_size         = 2
    }
  }

  persistent_directories {
    mount_path = "/home"
    gce_pd {
      size_gb   = 100
      disk_type = "pd-balanced"
    }
  }
}

resource "google_workstations_workstation_config" "ml" {
  workstation_config_id  = "ml"
  workstation_cluster_id = google_workstations_workstation_cluster.main.workstation_cluster_id
  location               = "europe-west1"
  running_timeout        = "0s"
  labels                 = local.prod

  host {
    gce_instance {
      machine_type = "n1-standard-8"
      accelerators {
        type  = "nvidia-tesla-t4"
        count = 1
      }
    }
  }
}

resource "google_workstations_workstation" "alice" {
  workstation_id         = "alice"
  workstation_config_id  = google_workstations_workstation_config.ml.workstation_config_id
  workstation_cluster_id = google_workstations_workstation_cluster.main.workstation_cluster_id
  location               = "europe-west1"
  labels                 = local.prod
}

resource "google_workstations_workstation" "bob" {
  workstation_id         = "bob"
  workstation_config_id  = google_workstations_workstation_config.ml.workstation_config_id
  workstation_cluster_id = google_workstations_workstation_cluster.main.workstation_cluster_id
  location               = "europe-west1"
  labels                 = local.prod
}

# ---------------------------------------------------------------------------
# Certificate Manager
# ---------------------------------------------------------------------------

resource "google_certificate_manager_dns_authorization" "example" {
  name   = "example"
  domain = "example.example"
}

resource "google_certificate_manager_certificate" "wildcard" {
  name   = "wildcard"
  labels = local.prod

  managed {
    domains            = ["*.example.example"]
    dns_authorizations = [google_certificate_manager_dns_authorization.example.id]
  }
}

resource "google_certificate_manager_certificate" "internal" {
  name     = "internal"
  location = "europe-west1"
  labels   = local.prod

  self_managed {
    pem_certificate = "-----BEGIN CERTIFICATE-----"
    pem_private_key = "-----BEGIN PRIVATE KEY-----"
  }
}

resource "google_certificate_manager_certificate_map" "main" {
  name = "main"
}

resource "google_certificate_manager_certificate_map_entry" "wildcard" {
  name         = "wildcard"
  map          = google_certificate_manager_certificate_map.main.name
  certificates = [google_certificate_manager_certificate.wildcard.id]
  hostname     = "*.example.example"
}

resource "google_certificate_manager_trust_config" "partners" {
  name     = "partners"
  location = "global"

  trust_stores {
    trust_anchors {
      pem_certificate = "-----BEGIN CERTIFICATE-----"
    }
  }
}

# ---------------------------------------------------------------------------
# Identity Platform
# ---------------------------------------------------------------------------

resource "google_identity_platform_config" "default" {
  project                  = "example-platform"
  autodelete_anonymous_users = true

  sign_in {
    email {
      enabled = true
    }
  }

  mfa {
    state = "ENABLED"
    enabled_providers = ["PHONE_SMS"]
  }
}

resource "google_identity_platform_tenant" "partners" {
  display_name = "partners"
}

# ---------------------------------------------------------------------------
# Cloud Deploy
# ---------------------------------------------------------------------------

resource "google_clouddeploy_target" "dev" {
  name     = "dev"
  location = "europe-west1"
  run {
    location = "projects/example-platform/locations/europe-west1"
  }
}

resource "google_clouddeploy_target" "staging" {
  name     = "staging"
  location = "europe-west1"
  run {
    location = "projects/example-platform/locations/europe-west1"
  }
}

resource "google_clouddeploy_target" "prod" {
  name     = "prod"
  location = "europe-west1"
  run {
    location = "projects/example-platform/locations/europe-west1"
  }
}

resource "google_clouddeploy_delivery_pipeline" "app" {
  name     = "app"
  location = "europe-west1"

  serial_pipeline {
    stages {
      target_id = google_clouddeploy_target.dev.name
    }
    stages {
      target_id = google_clouddeploy_target.staging.name
    }
    stages {
      target_id = google_clouddeploy_target.prod.name
    }
  }
}

resource "google_clouddeploy_delivery_pipeline" "sandbox" {
  name     = "sandbox"
  location = "europe-west1"

  serial_pipeline {
    stages {
      target_id = google_clouddeploy_target.dev.name
    }
  }
}
