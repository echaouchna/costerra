include "root" {
  path   = find_in_parent_folders("root.hcl")
  expose = true
}

terraform {
  source = "../../../modules//app"
}

dependency "network" {
  config_path = "../network"
  mock_outputs = {
    subnet = "mock-subnet"
  }
}

inputs = {
  subnet       = dependency.network.outputs.subnet
  machine_type = "n2-standard-4"
  replicas     = 2
  name         = "app-${include.root.locals.env}"
}
