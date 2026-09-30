#!/usr/bin/env bash
set -euo pipefail

API_URL="http://127.0.0.1:3000/api"

# 1️⃣ Login and grab token
echo "🔑 Logging in..."
TOKEN=$(curl -s -X POST $API_URL/local-auth/login \
  -H "Content-Type: application/json" \
  -d '{"email":"alice@tidasone.com","password":"secret123"}' | jq -r '.token')

echo "✅ Got token: ${TOKEN:0:20}..."

# 2️⃣ Users
echo -e "\n👤 Users list:"
curl -i -H "Authorization: Bearer $TOKEN" \
     $API_URL/users

echo -e "\n👤 Creating new user..."
curl -i -X POST $API_URL/users \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"username":"bob","email":"bob@example.com"}'

# 3️⃣ Inventory
echo -e "\n📦 Inventory list:"
curl -i -H "Authorization: Bearer $TOKEN" \
     $API_URL/inventory

echo -e "\n📥 Bulk importing inventory..."
curl -i -X POST $API_URL/inventory/bulk-import \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"items":[
    {"name":"Engine Core","description":"Fusion drive","quantity":5,"category":"engines","unit":"pcs","threshold":2},
    {"name":"Oxygen Tank","description":"Life support","quantity":10,"category":"supplies","unit":"tanks","threshold":5}
  ]}'

echo -e "\n🔍 Searching inventory (category=engines):"
curl -i -H "Authorization: Bearer $TOKEN" \
     "$API_URL/inventory/search?category=engines"

# 4️⃣ Packages
echo -e "\n📦 Listing packages..."
curl -i -H "Authorization: Bearer $TOKEN" \
     $API_URL/packages

echo -e "\n📦 Creating package (linked to first inventory item)..."
FIRST_INV_ID=$(curl -s -H "Authorization: Bearer $TOKEN" $API_URL/inventory | jq -r '.[0].id')
curl -i -X POST $API_URL/packages \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"inventory_item_id\":\"$FIRST_INV_ID\",\"status\":\"pending\",\"location\":\"Mars Base\",\"description\":\"Test package\"}"

# 5️⃣ Assets
echo -e "\n🛰️ Listing assets..."
curl -i -H "Authorization: Bearer $TOKEN" \
     $API_URL/assets

echo -e "\n🛰️ Creating asset..."
curl -i -X POST $API_URL/assets \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"name":"Drone 7","description":"Surveillance drone","location":"Hangar","status":"idle"}'

echo -e "\n✅ System test finished!"

