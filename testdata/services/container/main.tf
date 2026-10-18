provider "google" {
  project = "example-platform"
  region  = "europe-west1"
}

locals {
  prod = { env = "prod", team = "platform" }
  dev  = { env = "dev", team = "platform" }
}

# ---------------------------------------------------------------------------
# Production: regional Standard cluster with separately managed node pools.
# ---------------------------------------------------------------------------

resource "google_container_cluster" "prod" {
  name                     = "prod"
  location                 = "europe-west1"
  remove_default_node_pool = true
  initial_node_count       = 1
  resource_labels          = local.prod

  release_channel {
    channel = "REGULAR"
  }

  private_cluster_config {
    enable_private_nodes    = true
    enable_private_endpoint = false
    master_ipv4_cidr_block  = "172.16.0.0/28"
  }
}

resource "google_container_node_pool" "prod_general" {
  name    = "general"
  cluster = google_container_cluster.prod.id

  autoscaling {
    min_node_count = 1
    max_node_count = 5
  }

  node_config {
    machine_type = "n2-standard-8"
    disk_type    = "pd-ssd"
    disk_size_gb = 200
    labels       = local.prod
  }
}

resource "google_container_node_pool" "prod_gpu" {
  name           = "gpu"
  cluster        = google_container_cluster.prod.id
  node_locations = ["europe-west1-b"]
  node_count     = 1

  node_config {
    machine_type = "n1-standard-8"

    guest_accelerator {
      type  = "nvidia-tesla-t4"
      count = 1
    }
  }
}

resource "google_container_node_pool" "prod_batch" {
  name               = "batch"
  cluster            = google_container_cluster.prod.id
  initial_node_count = 1

  autoscaling {
    total_min_node_count = 0
    total_max_node_count = 12
  }

  node_config {
    machine_type = "c3-standard-8-lssd"
    spot         = true

    ephemeral_storage_local_ssd_config {
      local_ssd_count = 2
    }
  }
}

# ---------------------------------------------------------------------------
# Development: regional cluster that keeps its default pool, Extended channel,
# public nodes, plus an unlabeled fixed-size pool (inherits env from cluster).
# ---------------------------------------------------------------------------

resource "google_container_cluster" "dev" {
  name               = "dev"
  location           = "europe-west1"
  initial_node_count = 1
  resource_labels    = local.dev

  release_channel {
    channel = "EXTENDED"
  }

  node_config {
    machine_type = "e2-standard-4"
  }
}

resource "google_container_node_pool" "apps" {
  name       = "apps"
  cluster    = google_container_cluster.dev.name
  location   = "europe-west1"
  node_count = 2

  node_config {
    machine_type = "n2-standard-4"
    disk_size_gb = 50
  }
}

# ---------------------------------------------------------------------------
# Sandbox: small multi-zonal cluster with inline node pools (Linux + Windows).
# ---------------------------------------------------------------------------

resource "google_container_cluster" "sandbox" {
  name            = "sandbox"
  location        = "europe-west1-b"
  node_locations  = ["europe-west1-c"]
  resource_labels = { env = "sandbox" }

  private_cluster_config {
    enable_private_nodes   = true
    master_ipv4_cidr_block = "172.16.1.0/28"
  }

  node_pool {
    name       = "system"
    node_count = 1

    node_config {
      machine_type = "e2-small"
      disk_size_gb = 30
    }
  }

  node_pool {
    name       = "windows"
    node_count = 1

    node_config {
      machine_type = "n2-standard-2"
      image_type   = "WINDOWS_LTSC_CONTAINERD"
      spot         = true
    }
  }
}

# ---------------------------------------------------------------------------
# Autopilot clusters (Pod requests come from costerra-usage.yml).
# ---------------------------------------------------------------------------

resource "google_container_cluster" "autopilot_prod" {
  name             = "autopilot-prod"
  location         = "europe-west1"
  enable_autopilot = true
  resource_labels  = local.prod
}

resource "google_container_cluster" "autopilot_dev" {
  name             = "autopilot-dev"
  location         = "europe-west1"
  enable_autopilot = true
  resource_labels  = local.dev
}

resource "google_gke_hub_membership" "prod" {
  membership_id = "prod"
  endpoint {
    gke_cluster {
      resource_link = "//container.googleapis.com/${google_container_cluster.prod.id}"
    }
  }
}
