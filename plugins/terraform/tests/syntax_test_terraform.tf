# SYNTAX TEST "Packages/Terraform/Terraform.sublime-syntax"

terraform {
# <- keyword.declaration.terraform - entity.name.type
  required_version = ">= 1.5"
# ^^^^^^^^^^^^^^^^ variable.declaration.terraform variable.other.readwrite.terraform
  required_providers {
# ^^^^^^^^^^^^^^^^^^ keyword.declaration.terraform entity.name.type.terraform
    aws = {
      source = "hashicorp/aws"
#     ^^^^^^ meta.mapping.key.terraform string.unquoted.terraform
#              ^^^^^^^^^^^^^^^ string.quoted.double.terraform
    }
  }
}

resource "aws_instance" "web" {
# <- keyword.declaration.terraform - entity.name.type
#        ^^^^^^^^^^^^^^ meta.block.head.terraform meta.string.terraform
#         ^^^^^^^^^^^^ support.type.terraform
#                        ^^^ entity.name.label.terraform
#                             ^ meta.block.body.terraform punctuation.section.block.begin.terraform - meta.block.head
  count         = var.enabled ? 1 : 0
# ^^^^^ keyword.control.conditional.terraform
#                 ^^^ variable.language.terraform
#                    ^ punctuation.accessor.dot.terraform
#                     ^^^^^^^ variable.other.member.terraform
#                             ^ keyword.operator.ternary.terraform
  ami           = data.aws_ami.ubuntu.id
#                 ^^^^ variable.language.terraform
#                      ^^^^^^^ variable.other.member.terraform
  instance_type = local.sizes[terraform.workspace]
#                 ^^^^^ variable.language.terraform
#                             ^^^^^^^^^ variable.language.terraform
  user_data     = file("${path.module}/init.sh")
#                 ^^^^ support.function.builtin.terraform
#                       ^^ punctuation.section.interpolation.begin.terraform
#                         ^^^^ variable.language.terraform
#                              ^^^^^^ variable.other.member.terraform
#                                    ^ punctuation.section.interpolation.end.terraform

  root_block_device {
# ^^^^^^^^^^^^^^^^^ meta.block.body.terraform meta.block.head.terraform
# ^^^^^^^^^^^^^^^^^ keyword.declaration.terraform entity.name.type.terraform
    volume_size = 20
#                 ^^ constant.numeric.value.terraform
    encrypted   = true
#                 ^^^^ constant.language.boolean.true.terraform
  }

  dynamic "ingress" {
# ^^^^^^^ entity.name.type.terraform
#         ^^^^^^^^^ string.quoted.double.terraform
    for_each = var.ingress_rules
#   ^^^^^^^^ keyword.control.loop.for.terraform
    content {
#   ^^^^^^^ entity.name.type.terraform
      from_port = ingress.value.port
#                 ^^^^^^^ variable.other.readwrite.terraform
      cidr      = each.key
#                 ^^^^ variable.language.terraform
    }
  }

  lifecycle {
# ^^^^^^^^^ entity.name.type.terraform
    ignore_changes = [tags]
  }

  tags = merge(local.common_tags, { Name = "web-${count.index}" })
#        ^^^^^ support.function.builtin.terraform
#                                   ^^^^ meta.mapping.key.terraform
#                                                 ^^^^^ variable.language.terraform
}

variable "ingress_rules" {
# <- keyword.declaration.terraform
#         ^^^^^^^^^^^^^ entity.name.label.terraform
  type = list(object({
#        ^^^^ storage.type.terraform
#             ^^^^^^ storage.type.terraform
    port     = number
#              ^^^^^^ storage.type.terraform
    cidrs    = optional(list(string), [])
#              ^^^^^^^^ support.function.builtin.terraform
#                       ^^^^ storage.type.terraform
#                            ^^^^^^ storage.type.terraform
    labels   = map(string)
#              ^^^ storage.type.terraform
    pair     = tuple([string, bool])
#              ^^^^^ storage.type.terraform
#                     ^^^^^^ storage.type.terraform
#                             ^^^^ storage.type.terraform
    anything = any
#              ^^^ storage.type.terraform
  }))
  default = []

  validation {
# ^^^^^^^^^^ entity.name.type.terraform
    condition     = alltrue([for r in var.ingress_rules : r.port > 0])
#                   ^^^^^^^ support.function.builtin.terraform
#                            ^^^ keyword.control.loop.for.terraform
#                                ^ variable.other.readwrite.terraform
#                                  ^^ keyword.operator.iteration.in.terraform
#                                                                ^ keyword.operator.comparison.terraform
    error_message = "Ports must be positive."
#                   ^^^^^^^^^^^^^^^^^^^^^^^^^ string.quoted.double.terraform
  }
}

locals {
# <- keyword.declaration.terraform - entity.name.type
  upper_names = { for k, v in var.users : k => upper(v.name) if v.active }
#               ^ punctuation.section.braces.begin.terraform
#                 ^^^ keyword.control.loop.for.terraform
#                          ^^ keyword.operator.iteration.in.terraform
#                                           ^^ punctuation.separator.key-value.terraform
#                                              ^^^^^ support.function.builtin.terraform
#                                                            ^^ keyword.control.conditional.terraform
  all_ids     = [for s in module.vpc.subnets : s.id]
#                         ^^^^^^ variable.language.terraform
  greeting    = "%{ if var.name != "" }Hello, ${var.name}%{ else }Hi%{ endif }"
#                ^^ punctuation.section.interpolation.begin.terraform
#                   ^^ keyword.control.conditional.if.terraform
#                                                           ^^^^ keyword.control.conditional.else.terraform
#                                                                      ^^^^^ keyword.control.conditional.end.terraform
  custom      = provider::assert::cidr("10.0.0.0/8")
#                                 ^^^^ variable.function.terraform
}

module "vpc" {
# <- keyword.declaration.terraform
#       ^^^ entity.name.label.terraform
  source  = "terraform-aws-modules/vpc/aws"
  version = "~> 5.0"
  azs     = slice(data.aws_availability_zones.available.names, 0, 3)
#           ^^^^^ support.function.builtin.terraform
}

output "summary" {
# <- keyword.declaration.terraform
  value = <<-EOT
#         ^^^ keyword.operator.heredoc.terraform
#            ^^^ keyword.control.heredoc.terraform
    Instance: ${aws_instance.web[0].id}
#   ^^^^^^^^^ string.unquoted.heredoc.terraform
#             ^^ punctuation.section.interpolation.begin.terraform
#               ^^^^^^^^^^^^ - string
    %{~ for ip in local.all_ids ~}
#   ^^ punctuation.section.interpolation.begin.terraform
#     ^ keyword.operator.template.trim.left.terraform
#       ^^^ keyword.control.loop.for.terraform
    - ${ip}
    %{~ endfor ~}
#       ^^^^^^ keyword.control.loop.end.terraform
    EOT
#   ^^^ keyword.control.heredoc.terraform
  sensitive = false
# ^^^^^^^^^ variable.declaration.terraform
}

moved {
# <- keyword.declaration.terraform - entity.name.type
  from = aws_instance.old
  to   = aws_instance.web
}

import {
# <- keyword.declaration.terraform - entity.name.type
  to = aws_instance.web
  id = "i-0123456789"
}

policy = jsonencode({
  "Version": "2012-10-17",
# ^^^^^^^^^ meta.mapping.key.terraform string.quoted.double.terraform
#          ^ punctuation.separator.key-value.terraform
#            ^^^^^^^^^^^^ string.quoted.double.terraform
#                        ^ punctuation.separator.terraform
  "Statement": [{ Effect = "Allow" }]
# ^^^^^^^^^^^ meta.mapping.key.terraform
#                 ^^^^^^ meta.mapping.key.terraform string.unquoted.terraform
})

// tfvars-style assignments
region        = "us-east-1"
# <- variable.declaration.terraform variable.other.readwrite.terraform
#             ^ keyword.operator.assignment.terraform
instance_count = 3
#                ^ constant.numeric.value.terraform
cidr_blocks   = ["10.0.0.0/16", "10.1.0.0/16"]
#                ^^^^^^^^^^^^^ meta.brackets.terraform string.quoted.double.terraform
#                 ^^^^^^^^ constant.numeric.ip-address.v4.terraform
enable_nat    = null
#               ^^^^ constant.language.null.terraform
/* block
   comment */
# <- comment.block.terraform
