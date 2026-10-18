variable "name" {}
variable "machine_type" {}
variable "disk_size" { type = number }
variable "disk_type" {}
variable "zone" {}
variable "labels" { type = map(string) }

resource "google_compute_instance" "this" {
  name         = var.name
  machine_type = var.machine_type
  zone         = var.zone
  labels       = var.labels

  boot_disk {
    initialize_params {
      size  = var.disk_size
      type  = var.disk_type
      image = "debian-cloud/debian-12"
    }
  }

  dynamic "scratch_disk" {
    for_each = var.name == "api" ? [1] : []
    content {
      interface = "NVME"
    }
  }

  network_interface {
    network = "default"
    access_config {}
  }
}

output "id" {
  value = google_compute_instance.this.id
}
