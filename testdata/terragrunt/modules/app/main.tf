variable "project" {}
variable "env" {}
variable "name" { default = "app" }
variable "labels" { type = map(string) }
variable "subnet" {}
variable "machine_type" { default = "e2-standard-2" }
variable "replicas" { default = 1 }

resource "google_compute_instance" "app" {
  count        = var.replicas
  name         = "${var.name}-${count.index}"
  machine_type = var.machine_type
  labels       = var.labels

  boot_disk {
    initialize_params {
      size = 30
      type = "pd-balanced"
    }
  }

  network_interface {
    subnetwork = var.subnet
  }
}

output "names" {
  value = google_compute_instance.app[*].name
}
