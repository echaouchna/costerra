provider "google" {
  project = "example-data"
  region  = "europe-west1"
}

locals {
  prod = { env = "prod", team = "data" }
  dev  = { env = "dev", team = "data" }
}

# ---------------------------------------------------------------- BigQuery

resource "google_bigquery_dataset" "analytics" {
  dataset_id = "analytics"
  location   = "EU"
  labels     = local.prod
}

resource "google_bigquery_dataset" "raw" {
  dataset_id            = "raw"
  location              = "EU"
  storage_billing_model = "PHYSICAL"
  max_time_travel_hours = "168"
  labels                = local.prod
}

resource "google_bigquery_dataset" "sandbox" {
  dataset_id = "sandbox"
  location   = "europe-west1"
  labels     = local.dev
}

resource "google_bigquery_dataset" "scratch" {
  dataset_id                  = "scratch"
  location                    = "europe-west1"
  default_table_expiration_ms = 604800000
  labels                      = local.dev
}

resource "google_bigquery_table" "events" {
  dataset_id = google_bigquery_dataset.analytics.dataset_id
  table_id   = "events"
  labels     = local.prod

  time_partitioning {
    type = "DAY"
  }
}

resource "google_bigquery_table" "daily_kpis" {
  dataset_id = google_bigquery_dataset.analytics.dataset_id
  table_id   = "daily_kpis"

  view {
    query          = "SELECT DATE(ts) d, COUNT(*) n FROM analytics.events GROUP BY 1"
    use_legacy_sql = false
  }
}

resource "google_bigquery_table" "landing_files" {
  dataset_id = google_bigquery_dataset.raw.dataset_id
  table_id   = "landing_files"

  external_data_configuration {
    autodetect    = true
    source_format = "PARQUET"
    source_uris   = ["gs://example-landing/*.parquet"]
  }
}

resource "google_bigquery_reservation" "prod" {
  name          = "prod"
  location      = "EU"
  edition       = "ENTERPRISE"
  slot_capacity = 300

  autoscale {
    max_slots = 400
  }
}

resource "google_bigquery_capacity_commitment" "prod" {
  capacity_commitment_id = "prod-annual"
  location               = "EU"
  slot_count             = 100
  plan                   = "ANNUAL"
  edition                = "ENTERPRISE"
}

resource "google_bigquery_reservation_assignment" "prod" {
  assignee    = "projects/example-data"
  job_type    = "QUERY"
  reservation = google_bigquery_reservation.prod.id
  location    = "EU"
}

resource "google_bigquery_reservation" "dev" {
  name          = "dev"
  location      = "europe-west1"
  edition       = "ENTERPRISE_PLUS"
  slot_capacity = 100

  autoscale {
    max_slots = 100
  }
}

resource "google_bigquery_capacity_commitment" "legacy_flex" {
  capacity_commitment_id = "legacy-flex"
  location               = "US"
  slot_count             = 100
  plan                   = "FLEX"
}

resource "google_bigquery_bi_reservation" "prod" {
  location = "EU"
  size     = 10737418240
}

resource "google_bigquery_data_transfer_config" "kpi_refresh" {
  display_name           = "kpi-refresh"
  location               = "europe-west1"
  data_source_id         = "scheduled_query"
  schedule               = "every 24 hours"
  destination_dataset_id = google_bigquery_dataset.sandbox.dataset_id
  params = {
    query = "SELECT 1"
  }
}

resource "google_bigquery_data_transfer_config" "gcs_import" {
  display_name           = "gcs-import"
  location               = "EU"
  data_source_id         = "google_cloud_storage"
  destination_dataset_id = google_bigquery_dataset.raw.dataset_id
  params = {
    data_path_template = "gs://example-landing/*.csv"
  }
}

resource "google_bigquery_connection" "lake" {
  connection_id = "lake"
  location      = "EU"
  cloud_resource {}
}

# ---------------------------------------------------------------- Dataflow

resource "google_dataflow_job" "clickstream" {
  name              = "clickstream-prod"
  template_gcs_path = "gs://dataflow-templates-europe-west1/latest/PubSub_Subscription_to_BigQuery"
  temp_gcs_location = "gs://example-dataflow/tmp"
  region            = "europe-west1"
  parameters = {
    inputSubscription = "projects/example-data/subscriptions/clicks"
    outputTableSpec   = "example-data:analytics.events"
  }
  labels = local.prod
}

resource "google_dataflow_flex_template_job" "orders_cdc" {
  provider                = google
  name                    = "orders-cdc-prod"
  container_spec_gcs_path = "gs://dataflow-templates-europe-west1/latest/flex/Datastream_to_BigQuery"
  region                  = "europe-west1"
  enable_streaming_engine = true
  machine_type            = "n2-standard-2"
  num_workers             = 2
  max_workers             = 6
  labels                  = local.prod
}

resource "google_dataflow_flex_template_job" "nightly_export" {
  name                    = "nightly-export-dev"
  container_spec_gcs_path = "gs://example-dataflow/templates/export.json"
  region                  = "europe-west1"
  machine_type            = "n2-standard-4"
  num_workers             = 5
  max_workers             = 20
  parameters = {
    diskSizeGb = 50
  }
  labels = local.dev
}

# ---------------------------------------------------------------- Dataproc

resource "google_dataproc_cluster" "dev" {
  name   = "spark-dev"
  region = "europe-west1"
  labels = local.dev

  cluster_config {
    master_config {
      num_instances = 1
      machine_type  = "n2-standard-4"
      disk_config {
        boot_disk_type    = "pd-balanced"
        boot_disk_size_gb = 100
      }
    }

    worker_config {
      num_instances = 5
      machine_type  = "n2-standard-8"
      disk_config {
        boot_disk_type    = "pd-balanced"
        boot_disk_size_gb = 200
      }
    }

    software_config {
      image_version = "2.2-debian12"
    }
  }
}

resource "google_dataproc_autoscaling_policy" "prod" {
  policy_id = "spark-prod"
  location  = "europe-west1"

  worker_config {
    min_instances = 2
    max_instances = 4
  }

  secondary_worker_config {
    min_instances = 2
    max_instances = 20
  }

  basic_algorithm {
    yarn_config {
      graceful_decommission_timeout = "30s"
      scale_up_factor               = 0.5
      scale_down_factor             = 0.5
    }
  }
}

resource "google_dataproc_cluster" "prod" {
  name   = "spark-prod"
  region = "europe-west1"
  labels = local.prod

  cluster_config {
    master_config {
      num_instances = 3
      machine_type  = "n2-highmem-4"
      disk_config {
        boot_disk_type    = "pd-ssd"
        boot_disk_size_gb = 200
      }
    }

    worker_config {
      num_instances = 2
      machine_type  = "n2-highmem-8"
      disk_config {
        boot_disk_size_gb = 500
        num_local_ssds    = 1
      }
    }

    preemptible_worker_config {
      num_instances  = 4
      preemptibility = "SPOT"
      disk_config {
        boot_disk_size_gb = 100
      }
    }

    autoscaling_config {
      policy_uri = google_dataproc_autoscaling_policy.prod.name
    }

    lifecycle_config {
      idle_delete_ttl = "7200s"
    }
  }
}

resource "google_dataproc_batch" "backfill" {
  batch_id = "orders-backfill"
  location = "europe-west1"
  labels   = local.dev

  runtime_config {
    properties = {
      "spark.executor.instances" = "4"
      "spark.executor.cores"     = "8"
      "spark.executor.memory"    = "24g"
    }
  }

  pyspark_batch {
    main_python_file_uri = "gs://example-jobs/backfill.py"
  }
}

# ---------------------------------------------------------------- Composer

resource "google_composer_environment" "prod" {
  name   = "airflow-prod"
  region = "europe-west1"
  labels = local.prod

  config {
    environment_size = "ENVIRONMENT_SIZE_MEDIUM"
    resilience_mode  = "HIGH_RESILIENCE"

    software_config {
      image_version = "composer-2.9.7-airflow-2.9.3"
    }

    workloads_config {
      scheduler {
        cpu        = 2
        memory_gb  = 7.5
        storage_gb = 5
        count      = 2
      }
      web_server {
        cpu        = 2
        memory_gb  = 7.5
        storage_gb = 5
      }
      worker {
        cpu        = 2
        memory_gb  = 7.5
        storage_gb = 5
        min_count  = 2
        max_count  = 6
      }
    }
  }
}

resource "google_composer_environment" "dev" {
  name   = "airflow-dev"
  region = "europe-west1"
  labels = local.dev

  config {
    environment_size = "ENVIRONMENT_SIZE_LARGE"

    software_config {
      image_version = "composer-3-airflow-2.10.2-build.7"
    }
  }
}

resource "google_composer_environment" "legacy" {
  name   = "airflow-legacy-dev"
  region = "europe-west1"
  labels = local.dev

  config {
    node_count = 3

    node_config {
      machine_type = "n1-standard-2"
      disk_size_gb = 50
    }

    software_config {
      image_version = "composer-1.20.12-airflow-1.10.15"
    }
  }
}

# ---------------------------------------------------------------- Data Fusion

resource "google_data_fusion_instance" "dev" {
  name   = "fusion-dev"
  region = "europe-west1"
  type   = "ENTERPRISE"
  labels = local.dev
}

resource "google_data_fusion_instance" "prod" {
  name   = "fusion-prod"
  region = "europe-west1"
  type   = "BASIC"
  labels = local.prod
}

# ---------------------------------------------------------------- Datastream

resource "google_datastream_connection_profile" "mysql" {
  display_name          = "orders-mysql"
  location              = "europe-west1"
  connection_profile_id = "orders-mysql"

  mysql_profile {
    hostname = "10.0.0.5"
    username = "datastream"
    password = "secret"
  }
}

resource "google_datastream_connection_profile" "bq" {
  display_name          = "bigquery"
  location              = "europe-west1"
  connection_profile_id = "bigquery"
  bigquery_profile {}
}

resource "google_datastream_stream" "orders" {
  stream_id    = "orders"
  display_name = "orders"
  location     = "europe-west1"
  labels       = local.prod

  source_config {
    source_connection_profile = google_datastream_connection_profile.mysql.id
    mysql_source_config {}
  }

  destination_config {
    destination_connection_profile = google_datastream_connection_profile.bq.id
    bigquery_destination_config {
      single_target_dataset {
        dataset_id = "example-data:raw"
      }
    }
  }

  backfill_all {}
}

# ---------------------------------------------------------------- Dataplex

resource "google_dataplex_datascan" "orders_quality" {
  location     = "europe-west1"
  data_scan_id = "orders-quality"
  labels       = local.prod

  data {
    resource = "//bigquery.googleapis.com/projects/example-data/datasets/raw/tables/orders"
  }

  execution_spec {
    trigger {
      schedule {
        cron = "0 3 * * *"
      }
    }
  }

  data_quality_spec {
    rules {
      dimension = "COMPLETENESS"
      column    = "order_id"
      non_null_expectation {}
    }
  }
}
