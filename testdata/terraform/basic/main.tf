terraform {
  backend "gcs" {}
}

provider "google" {
  project = var.project
  region  = var.region
}

variable "project" { default = "example-dev" }
variable "region" { default = "europe-west1" }
variable "env" { default = "dev" }

variable "vms" {
  type = map(object({
    machine_type = optional(string, "n1-standard-4")
    disk_size    = optional(number, 50)
    disk_type    = optional(string, "pd-ssd")
  }))
  default = {
    api    = {}
    worker = { machine_type = "e2-standard-2", disk_type = "pd-balanced" }
  }
}

locals {
  labels = { env = var.env, team = "platform" }
  zone   = "${var.region}-b"
}

module "vm" {
  source       = "./modules/vm"
  for_each     = var.vms
  name         = each.key
  machine_type = each.value.machine_type
  disk_size    = each.value.disk_size
  disk_type    = each.value.disk_type
  zone         = local.zone
  labels       = local.labels
}

resource "google_compute_disk" "orphan" {
  name = "orphan-data"
  type = "pd-ssd"
  size = 500
  zone = local.zone
}

resource "google_compute_address" "unused" {
  name = "unused-ip"
}

resource "google_compute_address" "static" {
  name = "trainer-ip"
}

resource "google_compute_instance" "gpu" {
  name         = "trainer"
  machine_type = "n1-standard-8"
  zone         = local.zone

  guest_accelerator {
    type  = "nvidia-tesla-t4"
    count = 1
  }

  scheduling {
    on_host_maintenance = "TERMINATE"
  }

  boot_disk {
    initialize_params {
      size  = 100
      image = "projects/windows-cloud/global/images/family/windows-2022"
    }
  }

  network_interface {
    network = "default"
    access_config {
      nat_ip = google_compute_address.static.address
    }
  }
}

resource "google_compute_instance_template" "web" {
  name_prefix  = "web-"
  machine_type = "n2-standard-2"

  disk {
    source_image = "debian-cloud/debian-12"
    disk_size_gb = 20
    disk_type    = "pd-balanced"
    boot         = true
  }

  network_interface {
    network = "default"
  }
}

resource "google_compute_region_instance_group_manager" "web" {
  name               = "web"
  base_instance_name = "web"
  region             = var.region

  version {
    instance_template = google_compute_instance_template.web.self_link_unique
  }
}

resource "google_compute_region_autoscaler" "web" {
  name   = "web"
  region = var.region
  target = google_compute_region_instance_group_manager.web.id

  autoscaling_policy {
    min_replicas = 3
    max_replicas = 10
  }
}

resource "google_compute_network" "vpc" {
  name = "vpc"
}
