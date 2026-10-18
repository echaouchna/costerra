provider "google" {
  project = "example-network-prod"
  region  = "europe-west1"
}

locals {
  prod = { env = "prod", team = "network" }
  dev  = { env = "dev", team = "network" }
}

# ---------------------------------------------------------------------------
# VPC, workloads and Cloud NAT
# ---------------------------------------------------------------------------

resource "google_compute_network" "main" {
  name                    = "main"
  auto_create_subnetworks = false
}

resource "google_compute_subnetwork" "app" {
  name          = "app"
  network       = google_compute_network.main.id
  ip_cidr_range = "10.10.0.0/20"
  region        = "europe-west1"
}

resource "google_compute_subnetwork" "proxy_only" {
  name          = "proxy-only"
  network       = google_compute_network.main.id
  ip_cidr_range = "10.129.0.0/23"
  region        = "europe-west1"
  purpose       = "REGIONAL_MANAGED_PROXY"
  role          = "ACTIVE"
}

resource "google_compute_instance" "worker" {
  count        = 4
  name         = "worker-${count.index}"
  machine_type = "e2-standard-2"
  zone         = "europe-west1-b"
  labels       = local.prod

  boot_disk {
    initialize_params {
      image = "debian-cloud/debian-12"
    }
  }

  network_interface {
    subnetwork = google_compute_subnetwork.app.id
  }
}

resource "google_compute_instance_template" "api" {
  name_prefix  = "api-"
  machine_type = "e2-medium"

  disk {
    source_image = "debian-cloud/debian-12"
    boot         = true
  }

  network_interface {
    subnetwork = google_compute_subnetwork.app.id
  }
}

resource "google_compute_region_instance_group_manager" "api" {
  name               = "api"
  region             = "europe-west1"
  base_instance_name = "api"
  target_size        = 6

  version {
    instance_template = google_compute_instance_template.api.id
  }

  named_port {
    name = "http"
    port = 8080
  }
}

resource "google_compute_router" "main" {
  name    = "main"
  network = google_compute_network.main.id
  region  = "europe-west1"
}

# Prod NAT: auto-allocated IPs for the 10 private VMs above.
resource "google_compute_router_nat" "prod" {
  name                               = "prod-nat"
  router                             = google_compute_router.main.name
  region                             = "europe-west1"
  nat_ip_allocate_option             = "AUTO_ONLY"
  source_subnetwork_ip_ranges_to_nat = "ALL_SUBNETWORKS_ALL_IP_RANGES"

  log_config {
    enable = true
    filter = "ERRORS_ONLY"
  }
}

# Dev NAT in another region: 4 reserved IPs for 3 small VMs, logs everything.
resource "google_compute_address" "dev_nat" {
  count  = 4
  name   = "dev-nat-${count.index}"
  region = "europe-west4"
}

resource "google_compute_router" "dev" {
  name    = "dev"
  network = google_compute_network.main.id
  region  = "europe-west4"
}

resource "google_compute_instance" "dev_box" {
  count        = 3
  name         = "dev-box-${count.index}"
  machine_type = "e2-small"
  zone         = "europe-west4-a"
  labels       = local.dev

  boot_disk {
    initialize_params {
      image = "debian-cloud/debian-12"
    }
  }

  network_interface {
    network = google_compute_network.main.id
  }
}

resource "google_compute_router_nat" "dev" {
  name                               = "dev-nat"
  router                             = google_compute_router.dev.name
  region                             = "europe-west4"
  nat_ip_allocate_option             = "MANUAL_ONLY"
  nat_ips                            = google_compute_address.dev_nat[*].self_link
  source_subnetwork_ip_ranges_to_nat = "ALL_SUBNETWORKS_ALL_IP_RANGES"

  log_config {
    enable = true
    filter = "ALL"
  }
}

resource "google_compute_router_nat" "hybrid_private" {
  name                               = "hybrid-private-nat"
  router                             = google_compute_router.main.name
  region                             = "europe-west1"
  type                               = "PRIVATE"
  source_subnetwork_ip_ranges_to_nat = "LIST_OF_SUBNETWORKS"

  subnetwork {
    name                    = google_compute_subnetwork.app.id
    source_ip_ranges_to_nat = ["ALL_IP_RANGES"]
  }
}

# ---------------------------------------------------------------------------
# Global external Application Load Balancer + Cloud CDN + Cloud Armor
# ---------------------------------------------------------------------------

resource "google_compute_global_address" "web" {
  name = "web"
}

resource "google_compute_health_check" "http" {
  name = "http"
  http_health_check {
    port = 8080
  }
}

resource "google_compute_security_policy" "edge" {
  name = "edge-waf"

  dynamic "rule" {
    for_each = range(24)
    content {
      action   = "deny(403)"
      priority = 1000 + rule.value
      match {
        versioned_expr = "SRC_IPS_V1"
        config {
          src_ip_ranges = ["192.0.2.${rule.value}/32"]
        }
      }
    }
  }

  rule {
    action   = "allow"
    priority = 2147483647
    match {
      versioned_expr = "SRC_IPS_V1"
      config {
        src_ip_ranges = ["*"]
      }
    }
  }
}

resource "google_compute_security_policy_rule" "block_bad_bots" {
  security_policy = google_compute_security_policy.edge.name
  priority        = 900
  action          = "deny(403)"
  match {
    expr {
      expression = "request.headers['user-agent'].contains('badbot')"
    }
  }
}

resource "google_compute_backend_service" "web" {
  name                  = "web"
  load_balancing_scheme = "EXTERNAL_MANAGED"
  protocol              = "HTTP"
  health_checks         = [google_compute_health_check.http.id]
  security_policy       = google_compute_security_policy.edge.id
  enable_cdn            = true

  cdn_policy {
    cache_mode  = "CACHE_ALL_STATIC"
    default_ttl = 3600
  }

  backend {
    group = google_compute_region_instance_group_manager.api.instance_group
  }
}

resource "google_compute_backend_service" "api" {
  name                  = "api"
  load_balancing_scheme = "EXTERNAL_MANAGED"
  protocol              = "HTTP"
  health_checks         = [google_compute_health_check.http.id]
  security_policy       = google_compute_security_policy.edge.id

  backend {
    group = google_compute_region_instance_group_manager.api.instance_group
  }
}

resource "google_storage_bucket" "assets" {
  name     = "example-assets"
  location = "EU"
}

resource "google_compute_backend_bucket" "assets" {
  name        = "assets"
  bucket_name = google_storage_bucket.assets.name
  enable_cdn  = true

  cdn_policy {
    cache_mode = "USE_ORIGIN_HEADERS"
  }
}

resource "google_compute_url_map" "web" {
  name            = "web"
  default_service = google_compute_backend_service.web.id

  host_rule {
    hosts        = ["api.example.example"]
    path_matcher = "api"
  }

  path_matcher {
    name            = "api"
    default_service = google_compute_backend_service.api.id
    path_rule {
      paths   = ["/static/*"]
      service = google_compute_backend_bucket.assets.id
    }
  }
}

resource "google_compute_managed_ssl_certificate" "web" {
  name = "web"
  managed {
    domains = ["example.example", "api.example.example"]
  }
}

resource "google_compute_target_https_proxy" "web" {
  name             = "web"
  url_map          = google_compute_url_map.web.id
  ssl_certificates = [google_compute_managed_ssl_certificate.web.id]
}

resource "google_compute_global_forwarding_rule" "web_https" {
  name                  = "web-https"
  load_balancing_scheme = "EXTERNAL_MANAGED"
  target                = google_compute_target_https_proxy.web.id
  ip_address            = google_compute_global_address.web.id
  port_range            = "443"
}

resource "google_compute_url_map" "redirect" {
  name = "http-redirect"
  default_url_redirect {
    https_redirect = true
    strip_query    = false
  }
}

resource "google_compute_target_http_proxy" "redirect" {
  name    = "http-redirect"
  url_map = google_compute_url_map.redirect.id
}

resource "google_compute_global_forwarding_rule" "web_http" {
  name                  = "web-http"
  load_balancing_scheme = "EXTERNAL_MANAGED"
  target                = google_compute_target_http_proxy.redirect.id
  ip_address            = google_compute_global_address.web.id
  port_range            = "80"
}

# ---------------------------------------------------------------------------
# Regional load balancers
# ---------------------------------------------------------------------------

# Legacy passthrough NLBs, one per game server shard (7 forwarding rules).
resource "google_compute_target_pool" "shard" {
  count  = 7
  name   = "shard-${count.index}"
  region = "europe-west1"
}

resource "google_compute_forwarding_rule" "shard" {
  count      = 7
  name       = "shard-${count.index}"
  region     = "europe-west1"
  target     = google_compute_target_pool.shard[count.index].id
  port_range = "7777"
}

resource "google_compute_region_health_check" "tcp" {
  name   = "tcp"
  region = "europe-west1"
  tcp_health_check {
    port = 5432
  }
}

resource "google_compute_region_backend_service" "db_proxy" {
  name                  = "db-proxy"
  region                = "europe-west1"
  load_balancing_scheme = "INTERNAL"
  health_checks         = [google_compute_region_health_check.tcp.id]
}

resource "google_compute_forwarding_rule" "db_ilb" {
  name                  = "db-ilb"
  region                = "europe-west1"
  load_balancing_scheme = "INTERNAL"
  backend_service       = google_compute_region_backend_service.db_proxy.id
  network               = google_compute_network.main.id
  subnetwork            = google_compute_subnetwork.app.id
  ports                 = ["5432"]
}

resource "google_compute_region_backend_service" "internal_api" {
  name                  = "internal-api"
  region                = "europe-west1"
  load_balancing_scheme = "INTERNAL_MANAGED"
  protocol              = "HTTP"
  health_checks         = [google_compute_region_health_check.tcp.id]
}

resource "google_compute_region_url_map" "internal_api" {
  name            = "internal-api"
  region          = "europe-west1"
  default_service = google_compute_region_backend_service.internal_api.id
}

resource "google_compute_region_target_http_proxy" "internal_api" {
  name    = "internal-api"
  region  = "europe-west1"
  url_map = google_compute_region_url_map.internal_api.id
}

resource "google_compute_forwarding_rule" "internal_api" {
  name                  = "internal-api"
  region                = "europe-west1"
  load_balancing_scheme = "INTERNAL_MANAGED"
  target                = google_compute_region_target_http_proxy.internal_api.id
  network               = google_compute_network.main.id
  subnetwork            = google_compute_subnetwork.app.id
  port_range            = "80"
  depends_on            = [google_compute_subnetwork.proxy_only]
}

resource "google_compute_region_backend_service" "partner_api" {
  name                  = "partner-api"
  region                = "europe-west1"
  load_balancing_scheme = "EXTERNAL_MANAGED"
  protocol              = "HTTP"
  health_checks         = [google_compute_region_health_check.tcp.id]
}

resource "google_compute_region_url_map" "partner_api" {
  name            = "partner-api"
  region          = "europe-west1"
  default_service = google_compute_region_backend_service.partner_api.id
}

resource "google_compute_region_target_http_proxy" "partner_api" {
  name    = "partner-api"
  region  = "europe-west1"
  url_map = google_compute_region_url_map.partner_api.id
}

resource "google_compute_forwarding_rule" "partner_api" {
  name                  = "partner-api"
  region                = "europe-west1"
  load_balancing_scheme = "EXTERNAL_MANAGED"
  network_tier          = "STANDARD"
  target                = google_compute_region_target_http_proxy.partner_api.id
  network               = google_compute_network.main.id
  port_range            = "80"
}

resource "google_compute_region_target_tcp_proxy" "redis" {
  name            = "redis"
  region          = "europe-west1"
  backend_service = google_compute_region_backend_service.db_proxy.id
}

resource "google_compute_forwarding_rule" "redis_proxy" {
  name                  = "redis-proxy"
  region                = "europe-west1"
  load_balancing_scheme = "INTERNAL_MANAGED"
  target                = google_compute_region_target_tcp_proxy.redis.id
  network               = google_compute_network.main.id
  subnetwork            = google_compute_subnetwork.app.id
  port_range            = "6379"
}

# Cross-region internal Application Load Balancer.
resource "google_compute_backend_service" "mesh" {
  name                  = "mesh"
  load_balancing_scheme = "INTERNAL_MANAGED"
  protocol              = "HTTP"
  health_checks         = [google_compute_health_check.http.id]
}

resource "google_compute_url_map" "mesh" {
  name            = "mesh"
  default_service = google_compute_backend_service.mesh.id
}

resource "google_compute_target_http_proxy" "mesh" {
  name    = "mesh"
  url_map = google_compute_url_map.mesh.id
}

resource "google_compute_global_forwarding_rule" "mesh" {
  name                  = "mesh"
  load_balancing_scheme = "INTERNAL_MANAGED"
  target                = google_compute_target_http_proxy.mesh.id
  network               = google_compute_network.main.id
  subnetwork            = google_compute_subnetwork.app.id
  port_range            = "80"
}

resource "google_compute_region_security_policy" "partner" {
  name   = "partner-api"
  region = "europe-west1"
  type   = "CLOUD_ARMOR"

  rules {
    action   = "allow"
    priority = 1000
    match {
      versioned_expr = "SRC_IPS_V1"
      config {
        src_ip_ranges = ["198.51.100.0/24"]
      }
    }
  }

  rules {
    action   = "deny(403)"
    priority = 2147483647
    match {
      versioned_expr = "SRC_IPS_V1"
      config {
        src_ip_ranges = ["*"]
      }
    }
  }
}

# ---------------------------------------------------------------------------
# Private Service Connect
# ---------------------------------------------------------------------------

resource "google_compute_global_address" "psc_apis" {
  name         = "pscapis"
  purpose      = "PRIVATE_SERVICE_CONNECT"
  address_type = "INTERNAL"
  address      = "10.200.0.10"
  network      = google_compute_network.main.id
}

resource "google_compute_global_forwarding_rule" "psc_apis" {
  name                  = "pscapis"
  target                = "all-apis"
  network               = google_compute_network.main.id
  ip_address            = google_compute_global_address.psc_apis.id
  load_balancing_scheme = ""
}

resource "google_compute_service_attachment" "internal_api" {
  name                  = "internal-api"
  region                = "europe-west1"
  connection_preference = "ACCEPT_AUTOMATIC"
  nat_subnets           = [google_compute_subnetwork.app.id]
  target_service        = google_compute_forwarding_rule.db_ilb.id
  enable_proxy_protocol = false
}

resource "google_compute_address" "psc_endpoint" {
  name         = "psc-partner"
  region       = "europe-west1"
  subnetwork   = google_compute_subnetwork.app.id
  address_type = "INTERNAL"
}

resource "google_compute_forwarding_rule" "psc_partner" {
  name                  = "psc-partner"
  region                = "europe-west1"
  target                = google_compute_service_attachment.internal_api.id
  network               = google_compute_network.main.id
  ip_address            = google_compute_address.psc_endpoint.id
  load_balancing_scheme = ""
}

# ---------------------------------------------------------------------------
# Hybrid connectivity: VPN, Interconnect, NCC
# ---------------------------------------------------------------------------

resource "google_compute_ha_vpn_gateway" "onprem" {
  name    = "onprem"
  region  = "europe-west1"
  network = google_compute_network.main.id
}

resource "google_compute_external_vpn_gateway" "onprem" {
  name            = "onprem"
  redundancy_type = "TWO_IPS_REDUNDANCY"
  interface {
    id         = 0
    ip_address = "203.0.113.10"
  }
  interface {
    id         = 1
    ip_address = "203.0.113.11"
  }
}

resource "google_compute_vpn_tunnel" "onprem" {
  count                           = 2
  name                            = "onprem-${count.index}"
  region                          = "europe-west1"
  vpn_gateway                     = google_compute_ha_vpn_gateway.onprem.id
  vpn_gateway_interface           = count.index
  peer_external_gateway           = google_compute_external_vpn_gateway.onprem.id
  peer_external_gateway_interface = count.index
  shared_secret                   = "not-a-secret"
  router                          = google_compute_router.main.id
}

resource "google_compute_network" "analytics" {
  name                    = "analytics"
  auto_create_subnetworks = false
}

resource "google_compute_ha_vpn_gateway" "analytics" {
  name    = "analytics"
  region  = "europe-west4"
  network = google_compute_network.analytics.id
}

resource "google_compute_vpn_tunnel" "to_analytics" {
  name                  = "to-analytics"
  region                = "europe-west1"
  vpn_gateway           = google_compute_ha_vpn_gateway.onprem.id
  vpn_gateway_interface = 0
  peer_gcp_gateway      = google_compute_ha_vpn_gateway.analytics.id
  shared_secret         = "not-a-secret"
  router                = google_compute_router.main.id
}

# Legacy Classic VPN to a branch office.
resource "google_compute_address" "branch_vpn" {
  name   = "branch-vpn"
  region = "europe-west1"
}

resource "google_compute_vpn_gateway" "branch" {
  name    = "branch"
  region  = "europe-west1"
  network = google_compute_network.main.id
}

resource "google_compute_forwarding_rule" "branch_esp" {
  name        = "branch-esp"
  region      = "europe-west1"
  ip_protocol = "ESP"
  ip_address  = google_compute_address.branch_vpn.address
  target      = google_compute_vpn_gateway.branch.id
}

resource "google_compute_forwarding_rule" "branch_udp500" {
  name        = "branch-udp500"
  region      = "europe-west1"
  ip_protocol = "UDP"
  port_range  = "500"
  ip_address  = google_compute_address.branch_vpn.address
  target      = google_compute_vpn_gateway.branch.id
}

resource "google_compute_vpn_tunnel" "branch" {
  name                    = "branch"
  region                  = "europe-west1"
  peer_ip                 = "198.51.100.20"
  shared_secret           = "not-a-secret"
  target_vpn_gateway      = google_compute_vpn_gateway.branch.id
  local_traffic_selector  = ["10.10.0.0/20"]
  remote_traffic_selector = ["192.168.0.0/16"]
  depends_on              = [google_compute_forwarding_rule.branch_esp, google_compute_forwarding_rule.branch_udp500]
}

resource "google_compute_interconnect" "dc1" {
  name                 = "dc1"
  interconnect_type    = "DEDICATED"
  link_type            = "LINK_TYPE_ETHERNET_10G_LR"
  requested_link_count = 2
  location             = "https://www.googleapis.com/compute/v1/projects/example-network-prod/global/interconnectLocations/ams-zone1-2"
  customer_name        = "Example Customer"
}

resource "google_compute_interconnect_attachment" "dc1" {
  name         = "dc1"
  region       = "europe-west1"
  type         = "DEDICATED"
  interconnect = google_compute_interconnect.dc1.id
  bandwidth    = "BPS_5G"
  router       = google_compute_router.main.id
  vlan_tag8021q = 100
}

resource "google_compute_interconnect_attachment" "colo" {
  name                     = "colo"
  region                   = "europe-west1"
  type                     = "PARTNER"
  edge_availability_domain = "AVAILABILITY_DOMAIN_1"
  router                   = google_compute_router.main.id
}

resource "google_network_connectivity_hub" "main" {
  name = "main"
}

resource "google_network_connectivity_spoke" "analytics" {
  name     = "analytics"
  location = "global"
  hub      = google_network_connectivity_hub.main.id
  linked_vpc_network {
    uri = google_compute_network.analytics.self_link
  }
}

resource "google_network_connectivity_spoke" "onprem_vpn" {
  name     = "onprem-vpn"
  location = "europe-west1"
  hub      = google_network_connectivity_hub.main.id
  linked_vpn_tunnels {
    uris                       = google_compute_vpn_tunnel.onprem[*].self_link
    site_to_site_data_transfer = true
  }
}

# ---------------------------------------------------------------------------
# Serverless VPC Access
# ---------------------------------------------------------------------------

resource "google_vpc_access_connector" "prod" {
  name          = "prod-connector"
  region        = "europe-west1"
  network       = google_compute_network.main.name
  ip_cidr_range = "10.8.0.0/28"
  min_instances = 2
  max_instances = 10
}

resource "google_vpc_access_connector" "dev" {
  name          = "dev-connector"
  region        = "europe-west1"
  network       = google_compute_network.main.name
  ip_cidr_range = "10.8.0.16/28"
  machine_type  = "e2-standard-4"
  min_instances = 3
  max_instances = 6
}

# ---------------------------------------------------------------------------
# Cloud DNS
# ---------------------------------------------------------------------------

resource "google_dns_managed_zone" "public" {
  name     = "example-example"
  dns_name = "example.example."
  labels   = local.prod
}

resource "google_dns_managed_zone" "internal_main" {
  name       = "internal-main"
  dns_name   = "internal.example.example."
  visibility = "private"
  labels     = local.dev
  private_visibility_config {
    networks {
      network_url = google_compute_network.main.id
    }
  }
}

resource "google_dns_managed_zone" "internal_analytics" {
  name       = "internal-analytics"
  dns_name   = "internal.example.example."
  visibility = "private"
  labels     = local.dev
  private_visibility_config {
    networks {
      network_url = google_compute_network.analytics.id
    }
  }
}

# ---------------------------------------------------------------------------
# Packet Mirroring and Secure Web Proxy
# ---------------------------------------------------------------------------

resource "google_compute_region_backend_service" "ids" {
  name                  = "ids-collector"
  region                = "europe-west1"
  load_balancing_scheme = "INTERNAL"
  health_checks         = [google_compute_region_health_check.tcp.id]
}

resource "google_compute_forwarding_rule" "ids_collector" {
  name                   = "ids-collector"
  region                 = "europe-west1"
  load_balancing_scheme  = "INTERNAL"
  backend_service        = google_compute_region_backend_service.ids.id
  network                = google_compute_network.main.id
  subnetwork             = google_compute_subnetwork.app.id
  all_ports              = true
  is_mirroring_collector = true
}

resource "google_compute_packet_mirroring" "app" {
  name   = "app-mirroring"
  region = "europe-west1"
  network {
    url = google_compute_network.main.id
  }
  collector_ilb {
    url = google_compute_forwarding_rule.ids_collector.id
  }
  mirrored_resources {
    subnetworks {
      url = google_compute_subnetwork.app.id
    }
  }
}

resource "google_network_security_gateway_security_policy" "egress" {
  name     = "egress"
  location = "europe-west1"
}

resource "google_network_services_gateway" "swp" {
  name                                 = "egress-proxy"
  location                             = "europe-west1"
  type                                 = "SECURE_WEB_GATEWAY"
  addresses                            = ["10.10.15.250"]
  ports                                = [443]
  gateway_security_policy              = google_network_security_gateway_security_policy.egress.id
  network                              = google_compute_network.main.id
  subnetwork                           = google_compute_subnetwork.app.id
  delete_swg_autogen_router_on_destroy = true
}
