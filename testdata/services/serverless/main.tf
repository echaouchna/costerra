provider "google" {
  project = "example-serverless"
  region  = "europe-west1"
}

locals {
  prod = { env = "prod", team = "platform" }
  dev  = { env = "dev", team = "platform" }
}

# ---- Cloud Run services -------------------------------------------------------

# Prod API: request-based billing, 2 warm instances, traffic from the usage file.
resource "google_cloud_run_v2_service" "api" {
  name     = "api"
  location = "europe-west1"
  labels   = local.prod

  template {
    scaling {
      min_instance_count = 2
      max_instance_count = 50
    }
    max_instance_request_concurrency = 40
    containers {
      image = "europe-docker.pkg.dev/example/api:1.0"
      resources {
        limits = {
          cpu    = "2"
          memory = "1Gi"
        }
        cpu_idle = true
      }
    }
  }
}

# Dev service: resources set without cpu_idle => CPU always allocated, 1 min instance.
resource "google_cloud_run_v2_service" "backoffice_dev" {
  name     = "backoffice-dev"
  location = "europe-west1"
  labels   = local.dev

  template {
    scaling {
      min_instance_count = 1
    }
    containers {
      image = "europe-docker.pkg.dev/example/backoffice:dev"
      resources {
        limits = {
          cpu    = "1000m"
          memory = "512Mi"
        }
      }
    }
  }
}

# Oversized prod service in a Tier 2 region (London).
resource "google_cloud_run_v2_service" "reports" {
  name     = "reports"
  location = "europe-west2"
  labels   = local.prod

  template {
    containers {
      image = "europe-docker.pkg.dev/example/reports:2.3"
      resources {
        limits = {
          cpu    = "4"
          memory = "16Gi"
        }
        cpu_idle = true
      }
    }
    containers {
      image = "europe-docker.pkg.dev/example/otel-collector:0.9"
      resources {
        limits = {
          cpu    = "500m"
          memory = "256Mi"
        }
        cpu_idle = true
      }
    }
  }
}

# Dev GPU inference service with zonal redundancy (default).
resource "google_cloud_run_v2_service" "inference_dev" {
  name     = "inference-dev"
  location = "europe-west1"
  labels   = local.dev

  template {
    scaling {
      min_instance_count = 1
      max_instance_count = 3
    }
    node_selector {
      accelerator = "nvidia-l4"
    }
    containers {
      image = "europe-docker.pkg.dev/example/llm:dev"
      resources {
        limits = {
          cpu              = "4"
          memory           = "16Gi"
          "nvidia.com/gpu" = "1"
        }
        cpu_idle = false
      }
    }
  }
}

# Legacy v1 service: always-allocated CPU via annotation and a warm instance.
resource "google_cloud_run_service" "legacy_dev" {
  name     = "legacy-dev"
  location = "europe-west1"

  metadata {
    labels = local.dev
  }

  template {
    metadata {
      annotations = {
        "autoscaling.knative.dev/minScale"  = "1"
        "autoscaling.knative.dev/maxScale"  = "5"
        "run.googleapis.com/cpu-throttling" = "false"
      }
    }
    spec {
      container_concurrency = 80
      containers {
        image = "gcr.io/example/legacy:1"
        resources {
          limits = {
            cpu    = "1000m"
            memory = "2Gi"
          }
        }
      }
    }
  }
}

# ---- Cloud Run jobs & worker pools ---------------------------------------------

resource "google_cloud_run_v2_job" "etl" {
  name     = "nightly-etl"
  location = "europe-west1"
  labels   = local.prod

  template {
    task_count  = 10
    parallelism = 5
    template {
      timeout     = "3600s"
      max_retries = 1
      containers {
        image = "europe-docker.pkg.dev/example/etl:1.4"
        resources {
          limits = {
            cpu    = "2"
            memory = "4Gi"
          }
        }
      }
    }
  }
}

resource "google_cloud_run_v2_worker_pool" "consumer_dev" {
  name     = "queue-consumer-dev"
  location = "europe-west1"
  labels   = local.dev

  scaling {
    manual_instance_count = 2
  }

  template {
    containers {
      image = "europe-docker.pkg.dev/example/consumer:dev"
      resources {
        limits = {
          cpu    = "1"
          memory = "1Gi"
        }
      }
    }
  }
}

# ---- Cloud Run functions ---------------------------------------------------------

resource "google_cloudfunctions_function" "thumbnails" {
  name                = "thumbnails"
  region              = "europe-west1"
  runtime             = "python312"
  available_memory_mb = 512
  entry_point         = "handler"
  trigger_http        = true
  labels              = local.prod
}

resource "google_cloudfunctions_function" "webhook_dev" {
  name                = "webhook-dev"
  region              = "europe-west1"
  runtime             = "nodejs20"
  available_memory_mb = 1024
  min_instances       = 2
  entry_point         = "handler"
  trigger_http        = true
  labels              = local.dev
}

resource "google_cloudfunctions2_function" "ingest" {
  name     = "ingest"
  location = "europe-west1"
  labels   = local.prod

  build_config {
    runtime     = "go122"
    entry_point = "Ingest"
  }

  service_config {
    available_memory                 = "1Gi"
    available_cpu                    = "1"
    max_instance_count               = 20
    max_instance_request_concurrency = 10
  }
}

resource "google_cloudfunctions2_function" "notifier_dev" {
  name     = "notifier-dev"
  location = "europe-west1"
  labels   = local.dev

  build_config {
    runtime     = "python312"
    entry_point = "notify"
  }

  service_config {
    available_memory   = "512M"
    min_instance_count = 3
  }
}

# ---- Pub/Sub -------------------------------------------------------------------------

resource "google_pubsub_topic" "events" {
  name                       = "events"
  labels                     = local.prod
  message_retention_duration = "86400s"
}

resource "google_pubsub_topic" "audit_dev" {
  name                       = "audit-dev"
  labels                     = local.dev
  message_retention_duration = "2678400s"
}

resource "google_pubsub_topic" "kinesis_import" {
  name   = "kinesis-import"
  labels = local.prod

  ingestion_data_source_settings {
    aws_kinesis {
      stream_arn          = "arn:aws:kinesis:eu-west-1:111111111111:stream/orders"
      consumer_arn        = "arn:aws:kinesis:eu-west-1:111111111111:stream/orders/consumer/gcp:1"
      aws_role_arn        = "arn:aws:iam::111111111111:role/pubsub"
      gcp_service_account = "pubsub-import@example-serverless.iam.gserviceaccount.com"
    }
  }
}

resource "google_pubsub_subscription" "events_worker" {
  name                 = "events-worker"
  topic                = google_pubsub_topic.events.id
  labels               = local.prod
  ack_deadline_seconds = 30

  retain_acked_messages      = true
  message_retention_duration = "604800s"
}

resource "google_pubsub_subscription" "events_bq" {
  name   = "events-to-bq"
  topic  = google_pubsub_topic.events.id
  labels = local.prod

  bigquery_config {
    table = "example-serverless.analytics.events"
  }
}

resource "google_pubsub_subscription" "audit_replay_dev" {
  name   = "audit-replay-dev"
  topic  = google_pubsub_topic.audit_dev.id
  labels = local.dev

  retain_acked_messages      = true
  message_retention_duration = "1209600s"
}

resource "google_pubsub_lite_reservation" "legacy" {
  name                = "legacy-reservation"
  region              = "europe-west1"
  throughput_capacity = 4
}

# ---- Scheduler / Tasks / Workflows / Eventarc / API Gateway ---------------------------

resource "google_cloud_scheduler_job" "jobs" {
  for_each = toset(["etl", "cleanup", "report", "warmup", "sync"])
  name     = "trigger-${each.key}"
  region   = "europe-west1"
  schedule = "0 3 * * *"
  paused   = each.key == "warmup"

  http_target {
    uri         = "https://example.com/${each.key}"
    http_method = "POST"
  }
}

resource "google_cloud_tasks_queue" "emails" {
  name     = "emails"
  location = "europe-west1"
}

resource "google_workflows_workflow" "orders" {
  name            = "orders"
  region          = "europe-west1"
  labels          = local.prod
  source_contents = <<-EOT
    main:
      steps:
        - done:
            return: "ok"
  EOT
}

resource "google_eventarc_trigger" "on_upload" {
  name     = "on-upload"
  location = "europe-west1"
  matching_criteria {
    attribute = "type"
    value     = "google.cloud.storage.object.v1.finalized"
  }
  destination {
    cloud_run_service {
      service = google_cloud_run_v2_service.api.name
      region  = "europe-west1"
    }
  }
}

resource "google_eventarc_message_bus" "bus" {
  location       = "europe-west1"
  message_bus_id = "example-bus"
}

resource "google_eventarc_pipeline" "pipeline" {
  location    = "europe-west1"
  pipeline_id = "orders-pipeline"

  destinations {
    workflow = google_workflows_workflow.orders.id
  }

  mediations {
    transformation {
      transformation_template = "message.removeFields([\"data.credit_card\"])"
    }
  }
}

resource "google_api_gateway_gateway" "public" {
  provider   = google
  api_config = "projects/example-serverless/locations/global/apis/public/configs/v1"
  gateway_id = "public-gw"
  region     = "europe-west1"
}

# ---- App Engine -------------------------------------------------------------------------

resource "google_app_engine_application" "app" {
  project     = "example-serverless"
  location_id = "europe-west"
}

resource "google_app_engine_standard_app_version" "frontend_dev" {
  service    = "frontend"
  version_id = "v1"
  runtime    = "python312"

  instance_class = "F2"

  automatic_scaling {
    standard_scheduler_settings {
      min_instances = 1
      max_instances = 5
    }
  }

  entrypoint {
    shell = "gunicorn -b :$PORT main:app"
  }

  deployment {
    zip {
      source_url = "https://storage.googleapis.com/example-dev/frontend.zip"
    }
  }
}

resource "google_app_engine_standard_app_version" "worker" {
  service    = "worker"
  version_id = "v1"
  runtime    = "go122"

  instance_class = "B2"

  manual_scaling {
    instances = 1
  }

  deployment {
    zip {
      source_url = "https://storage.googleapis.com/example/worker.zip"
    }
  }
}

resource "google_app_engine_flexible_app_version" "legacy_dev" {
  service    = "legacy-dev"
  version_id = "v1"
  runtime    = "custom"

  resources {
    cpu       = 2
    memory_gb = 4
    disk_gb   = 20
  }

  automatic_scaling {
    cool_down_period = "120s"
    cpu_utilization {
      target_utilization = 0.6
    }
  }

  deployment {
    container {
      image = "europe-docker.pkg.dev/example/legacy:dev"
    }
  }

  liveness_check {
    path = "/"
  }

  readiness_check {
    path = "/"
  }
}
