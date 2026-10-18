# A non-production project enrolled in Cloud Armor Enterprise Paygo.
provider "google" {
  project = "example-shop-dev"
  region  = "europe-west1"
}

resource "google_compute_project_cloud_armor_tier" "this" {
  cloud_armor_tier = "CA_ENTERPRISE_PAYGO"
}

resource "google_compute_security_policy" "shop" {
  name = "shop-dev"
}

resource "google_compute_health_check" "http" {
  name = "http"
  http_health_check {
    port = 80
  }
}

resource "google_compute_backend_service" "shop" {
  count           = 3
  name            = "shop-${count.index}"
  protocol        = "HTTP"
  health_checks   = [google_compute_health_check.http.id]
  security_policy = google_compute_security_policy.shop.id
}
