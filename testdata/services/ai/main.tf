provider "google" {
  region = "europe-west1"
}

locals {
  dev  = { env = "dev", team = "ml" }
  prod = { env = "prod", team = "ml" }
}

# ---- Vertex AI Workbench ------------------------------------------------------

resource "google_workbench_instance" "prod_analytics" {
  name     = "analytics"
  location = "europe-west1-b"
  labels   = local.prod

  gce_setup {
    machine_type = "n2-standard-8"
    boot_disk {
      disk_type    = "PD_BALANCED"
      disk_size_gb = 150
    }
    data_disks {
      disk_type    = "PD_SSD"
      disk_size_gb = 500
    }
    disable_public_ip = true
    metadata = {
      idle-timeout-seconds = "10800"
    }
  }
}

resource "google_workbench_instance" "dev_gpu" {
  name     = "dev-gpu"
  location = "europe-west1-b"
  labels   = local.dev

  gce_setup {
    machine_type = "n1-standard-8"
    accelerator_configs {
      type       = "NVIDIA_TESLA_T4"
      core_count = 1
    }
    data_disks {
      disk_type    = "PD_BALANCED"
      disk_size_gb = 200
    }
  }
}

# g2 machine types bundle their L4 GPU: the accelerator block must not be double counted.
resource "google_workbench_instance" "dev_l4" {
  name     = "dev-l4"
  location = "europe-west1-c"
  labels   = local.dev

  gce_setup {
    machine_type = "g2-standard-8"
    accelerator_configs {
      type       = "NVIDIA_L4"
      core_count = 1
    }
    metadata = {
      idle-timeout-seconds = "3600"
    }
  }
}

resource "google_workbench_instance" "dev_stopped" {
  name          = "dev-stopped"
  location      = "europe-west1-b"
  labels        = local.dev
  desired_state = "STOPPED"

  gce_setup {
    machine_type = "e2-standard-4"
  }
}

# ---- Legacy notebooks ------------------------------------------------------------

resource "google_notebooks_instance" "dev_legacy" {
  name              = "legacy-user-managed"
  location          = "europe-west1-b"
  machine_type      = "n1-standard-4"
  boot_disk_type    = "PD_BALANCED"
  data_disk_size_gb = 200
  labels            = local.dev

  vm_image {
    project      = "deeplearning-platform-release"
    image_family = "tf-latest-cpu"
  }
}

resource "google_notebooks_runtime" "dev_managed" {
  name     = "legacy-managed"
  location = "europe-west1"
  labels   = local.dev

  access_config {
    access_type   = "SINGLE_USER"
    runtime_owner = "data-scientist@example.com"
  }

  software_config {
    idle_shutdown = false
  }

  virtual_machine {
    virtual_machine_config {
      machine_type = "n1-standard-4"
      data_disk {
        initialize_params {
          disk_size_gb = 100
          disk_type    = "PD_STANDARD"
        }
      }
    }
  }
}

# ---- Colab Enterprise --------------------------------------------------------------

resource "google_colab_runtime_template" "gpu" {
  name         = "gpu-template"
  display_name = "T4 runtime"
  location     = "europe-west1"

  machine_spec {
    machine_type      = "n1-standard-4"
    accelerator_type  = "NVIDIA_TESLA_T4"
    accelerator_count = 1
  }

  data_persistent_disk_spec {
    disk_type    = "pd-balanced"
    disk_size_gb = 200
  }

  idle_shutdown_config {
    idle_timeout = "0s"
  }

  network_spec {
    enable_internet_access = true
  }
}

resource "google_colab_runtime" "dev" {
  name         = "dev-runtime"
  location     = "europe-west1"
  display_name = "dev runtime"
  runtime_user = "data-scientist@example.com"

  notebook_runtime_template_ref {
    notebook_runtime_template = google_colab_runtime_template.gpu.id
  }
}

# ---- Cloud TPU -------------------------------------------------------------------------

resource "google_tpu_v2_vm" "dev_v5e" {
  name            = "dev-v5e"
  zone            = "europe-west4-b"
  runtime_version = "v2-alpha-tpuv5-lite"
  accelerator_type = "v5litepod-8"
  labels          = local.dev
}

resource "google_tpu_v2_vm" "prod_v6e" {
  name            = "prod-v6e"
  zone            = "europe-west4-a"
  runtime_version = "v2-alpha-tpuv6e"
  labels          = local.prod

  accelerator_config {
    type     = "V6E"
    topology = "2x2"
  }
}

resource "google_tpu_v2_vm" "dev_v5p_spot" {
  name             = "dev-v5p"
  zone             = "us-east5-a"
  runtime_version  = "v2-alpha-tpuv5"
  accelerator_type = "v5p-8"
  labels           = local.dev

  scheduling_config {
    spot = true
  }
}

resource "google_tpu_node" "prod_v3" {
  name               = "prod-v3"
  zone               = "europe-west4-a"
  accelerator_type   = "v3-8"
  tensorflow_version = "2.12.0"
  labels             = local.prod
}

resource "google_tpu_node" "dev_v2_pod" {
  name               = "dev-v2-pod"
  zone               = "us-central1-b"
  accelerator_type   = "v2-32"
  tensorflow_version = "2.12.0"
  labels             = local.dev
}

# ---- Vector Search -------------------------------------------------------------------

resource "google_vertex_ai_index" "products" {
  display_name        = "products"
  region              = "europe-west1"
  index_update_method = "STREAM_UPDATE"
  labels              = local.dev

  metadata {
    contents_delta_uri = "gs://example-ml-embeddings/products"
    config {
      dimensions                  = 768
      approximate_neighbors_count = 150
      shard_size                  = "SHARD_SIZE_SMALL"
      distance_measure_type       = "DOT_PRODUCT_DISTANCE"
      algorithm_config {
        tree_ah_config {}
      }
    }
  }
}

resource "google_vertex_ai_index_endpoint" "search" {
  display_name            = "search"
  region                  = "europe-west1"
  public_endpoint_enabled = true
}

resource "google_vertex_ai_index_endpoint_deployed_index" "dev_products" {
  index_endpoint    = google_vertex_ai_index_endpoint.search.id
  index             = google_vertex_ai_index.products.id
  deployed_index_id = "dev_products"

  dedicated_resources {
    machine_spec {
      machine_type = "n1-standard-16"
    }
    min_replica_count = 2
    max_replica_count = 4
  }
}

resource "google_vertex_ai_index_endpoint_deployed_index" "prod_products" {
  index_endpoint    = google_vertex_ai_index_endpoint.search.id
  index             = google_vertex_ai_index.products.id
  deployed_index_id = "prod_products"

  automatic_resources {
    min_replica_count = 2
    max_replica_count = 6
  }
}

# ---- Feature Store ----------------------------------------------------------------------

resource "google_vertex_ai_featurestore" "dev_legacy" {
  name   = "dev_legacy"
  region = "europe-west1"
  labels = local.dev

  online_serving_config {
    fixed_node_count = 3
  }
}

resource "google_vertex_ai_featurestore_entitytype" "users" {
  name         = "users"
  featurestore = google_vertex_ai_featurestore.dev_legacy.id
}

resource "google_vertex_ai_feature_online_store" "dev_bigtable" {
  name   = "dev_bigtable"
  region = "europe-west1"
  labels = local.dev

  bigtable {
    auto_scaling {
      min_node_count         = 3
      max_node_count         = 5
      cpu_utilization_target = 60
    }
  }
}

resource "google_vertex_ai_feature_online_store" "prod_optimized" {
  name   = "prod_optimized"
  region = "europe-west1"
  labels = local.prod

  optimized {}
}

resource "google_vertex_ai_feature_online_store_featureview" "prod_users" {
  name                 = "prod_users"
  region               = "europe-west1"
  feature_online_store = google_vertex_ai_feature_online_store.prod_optimized.name

  sync_config {
    cron = "0 * * * *"
  }

  big_query_source {
    uri               = "bq://example-ml.features.users"
    entity_id_columns = ["user_id"]
  }
}

# ---- TensorBoard / Metadata ------------------------------------------------------------

resource "google_vertex_ai_tensorboard" "experiments" {
  display_name = "experiments"
  region       = "europe-west1"
  labels       = local.dev
}

resource "google_vertex_ai_metadata_store" "default" {
  name   = "default"
  region = "europe-west1"
}

# ---- Online prediction -----------------------------------------------------------------

resource "google_vertex_ai_deployment_resource_pool" "dev_pool" {
  name   = "dev-pool"
  region = "europe-west1"

  dedicated_resources {
    machine_spec {
      machine_type      = "n1-standard-4"
      accelerator_type  = "NVIDIA_TESLA_T4"
      accelerator_count = 1
    }
    min_replica_count = 2
    max_replica_count = 4
  }
}

resource "google_vertex_ai_endpoint_with_model_garden_deployment" "prod_gemma" {
  publisher_model_name = "publishers/google/models/gemma3@gemma-3-1b-it"
  location             = "us-central1"

  model_config {
    accept_eula = true
  }

  deploy_config {
    dedicated_resources {
      machine_spec {
        machine_type      = "g2-standard-12"
        accelerator_type  = "NVIDIA_L4"
        accelerator_count = 1
      }
      min_replica_count = 1
    }
  }
}

# ---- Free -------------------------------------------------------------------------------

resource "google_vertex_ai_dataset" "images" {
  display_name        = "images"
  region              = "europe-west1"
  metadata_schema_uri = "gs://google-cloud-aiplatform/schema/dataset/metadata/image_1.0.0.yaml"
}

resource "google_vertex_ai_endpoint" "serving" {
  name         = "serving"
  display_name = "serving"
  location     = "europe-west1"
}

resource "google_ml_engine_model" "legacy" {
  name    = "legacy_model"
  regions = ["europe-west1"]
}
