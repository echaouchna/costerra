variable "project" {}
variable "env" {}
variable "labels" { type = map(string) }

resource "google_compute_network" "vpc" {
  name                    = "vpc-${var.env}"
  auto_create_subnetworks = false
}

resource "google_compute_subnetwork" "main" {
  name          = "main-${var.env}"
  network       = google_compute_network.vpc.id
  ip_cidr_range = "10.0.0.0/20"
}

resource "google_compute_address" "nat" {
  count = var.env == "prod" ? 2 : 1
  name  = "nat-${count.index}"
}

output "subnet" {
  value = "subnet-${var.env}"
}
